use crate::{
    D2dContext,
    utility_overlay::{ColorPinMailbox, UtilityOverlay},
};
use anyhow::{Result, bail, ensure};
use orbment_core::{
    config::{ColorFormat, KeepAwakeDuration, KeepAwakeMode, TopmostTarget, UtilitiesConfig},
    i18n::tr,
    lifecycle::{ActivateTrigger, CoreContext, OrbmentModule, ServiceState, UiSurfaceState},
    utilities::{self, BrightnessService, KeepAwakeService},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{cell::Cell, collections::BTreeMap, rc::Rc, time::Duration};
use windows::Win32::Foundation::HWND;

pub struct UtilitiesModule {
    graphics: Rc<D2dContext>,
    wake: HWND,
    config: UtilitiesConfig,
    running: bool,
    awake: KeepAwakeService,
    brightness: Option<BrightnessService>,
    picker: Option<UtilityOverlay>,
    borders: BTreeMap<isize, UtilityOverlay>,
    color_pin_enabled: Rc<Cell<bool>>,
    color_pin: ColorPinMailbox,
}
impl UtilitiesModule {
    pub fn new(graphics: Rc<D2dContext>, wake: HWND) -> Self {
        let wake_value = wake.0 as usize;
        Self {
            graphics,
            wake,
            config: UtilitiesConfig::default(),
            running: false,
            awake: KeepAwakeService::new(move || crate::wake(HWND(wake_value as _))),
            brightness: None,
            picker: None,
            borders: BTreeMap::new(),
            color_pin_enabled: Rc::new(Cell::new(false)),
            color_pin: Rc::new(Cell::new(None)),
        }
    }
    pub fn set_color_pin_enabled(&self, enabled: bool) {
        self.color_pin_enabled.set(enabled);
        if !enabled {
            self.color_pin.set(None);
        }
    }
    pub fn take_color_pin(&self) -> Option<([u8; 3], i32, i32)> {
        self.color_pin.take()
    }
    pub fn is_awake(&self) -> bool {
        self.awake.is_active()
    }
    pub fn status(&self) -> Value {
        json!({"keep_awake_active":self.is_awake(),"monitors":self.brightness.as_ref().map(BrightnessService::snapshot).unwrap_or_default()})
    }
    pub fn display_changed(&mut self) -> Result<()> {
        if let Some(service) = &self.brightness {
            service.refresh()?;
        }
        Ok(())
    }
    pub fn maintenance(&mut self) -> Result<()> {
        if self
            .picker
            .as_ref()
            .is_some_and(|picker| !picker.is_alive())
        {
            self.picker = None;
        }
        self.borders.retain(|_, border| border.is_alive());
        Ok(())
    }
    pub fn apply_config(&mut self, ctx: &CoreContext) -> Result<()> {
        if !ctx.config.utilities.enabled {
            self.config = ctx.config.utilities.clone();
            return self.stop_service(ctx);
        }
        if let Some(brightness) = &self.brightness
            && self.config.brightness_debounce_ms != ctx.config.utilities.brightness_debounce_ms
        {
            brightness.set_debounce(Duration::from_millis(u64::from(
                ctx.config.utilities.brightness_debounce_ms,
            )))?;
        }
        self.config = ctx.config.utilities.clone();
        self.running = true;
        Ok(())
    }
    pub fn rebuild_graphics(&mut self, graphics: Rc<D2dContext>, _ctx: &CoreContext) -> Result<()> {
        self.graphics = graphics.clone();
        if let Some(picker) = &self.picker {
            picker.rebuild_graphics(graphics.clone());
        }
        for border in self.borders.values() {
            border.rebuild_graphics(graphics.clone());
        }
        Ok(())
    }
    fn brightness(&mut self) -> Result<&BrightnessService> {
        if self.brightness.is_none() {
            let wake = self.wake.0 as usize;
            self.brightness = Some(BrightnessService::new(
                Duration::from_millis(u64::from(self.config.brightness_debounce_ms)),
                move || crate::wake(HWND(wake as _)),
            )?);
        }
        Ok(self
            .brightness
            .as_ref()
            .expect("Brightness service initialized"))
    }
    pub fn command(&mut self, action: &str, params: Value, ctx: &CoreContext) -> Result<Value> {
        ensure!(self.running, "{}", tr("utilities.disabled"));
        let params = if params.is_null() { json!({}) } else { params };
        match action {
            "utilities.set_topmost" => {
                let args: TopmostArgs = serde_json::from_value(params)?;
                ensure!(
                    usize::from(args.hwnd.is_some())
                        + usize::from(args.title.is_some())
                        + usize::from(args.target.is_some())
                        <= 1,
                    "{}",
                    tr("config.invalid_value")
                );
                ensure!(
                    args.hwnd != Some(0) && args.title.as_ref().is_none_or(|s| !s.is_empty()),
                    "{}",
                    tr("config.invalid_value")
                );
                let change = utilities::set_topmost(
                    args.hwnd,
                    args.title.as_deref(),
                    args.target.unwrap_or(self.config.topmost_target),
                    args.enabled,
                    self.config.topmost_border_color,
                )?;
                self.borders.remove(&change.hwnd);
                if change.enabled && !change.native_border {
                    self.borders.insert(
                        change.hwnd,
                        UtilityOverlay::border(
                            self.graphics.clone(),
                            self.wake,
                            HWND(change.hwnd as *mut _),
                            self.config.topmost_border_color,
                        )?,
                    );
                }
                Ok(serde_json::to_value(change)?)
            }
            "utilities.pick_color" => {
                let args: PickerArgs = serde_json::from_value(params)?;
                let format = args.format.unwrap_or(self.config.color_format);
                match (args.x, args.y) {
                    (Some(x), Some(y)) => {
                        let color = utilities::screen_pixel(x, y)?;
                        let text = if format == ColorFormat::Custom {
                            color.format_template(&self.config.color_template)
                        } else {
                            color.format(format)
                        };
                        utilities::copy_text(self.wake.0 as isize, &text)?;
                        Ok(json!({"color":color,"text":text,"copied":true}))
                    }
                    (None, None) => {
                        self.picker = Some(UtilityOverlay::picker(
                            self.graphics.clone(),
                            self.wake,
                            format,
                            self.config.color_template.clone(),
                            self.color_pin_enabled.clone(),
                            self.color_pin.clone(),
                        )?);
                        Ok(json!({"active":true}))
                    }
                    _ => bail!(tr("config.invalid_value")),
                }
            }
            "utilities.keep_awake" => {
                let args: AwakeArgs = serde_json::from_value(params)?;
                if args.enabled.unwrap_or(!self.is_awake()) {
                    let mode = args.mode.unwrap_or(self.config.keep_awake_mode);
                    let duration = args.duration.unwrap_or(self.config.keep_awake_duration);
                    let pid = args.process_id.or(self.config.keep_awake_process_id);
                    ensure!(pid != Some(0), "{}", tr("config.invalid_value"));
                    self.awake.set(mode, duration, pid)?;
                } else {
                    self.awake.stop();
                }
                Ok(self.status())
            }
            "utilities.display_brightness" => {
                let args: BrightnessArgs = serde_json::from_value(params)?;
                ensure!(
                    args.percent.is_none_or(|p| p <= 100)
                        && args.delta.is_none_or(|d| (-100..=100).contains(&d)),
                    "{}",
                    tr("config.invalid_value")
                );
                ensure!(
                    args.id.is_some() == args.percent.is_some()
                        && !(args.percent.is_some() && args.delta.is_some()),
                    "{}",
                    tr("config.invalid_value")
                );
                let service = self.brightness()?;
                if args.refresh {
                    service.refresh()?;
                }
                if let (Some(id), Some(percent)) = (args.id, args.percent) {
                    service.set(id, percent)?;
                }
                if let Some(delta) = args.delta {
                    service.adjust(delta)?;
                }
                let mut result = self.status();
                result["queued"] =
                    json!(args.refresh || args.percent.is_some() || args.delta.is_some());
                Ok(result)
            }
            "utilities.brightness_up" | "utilities.brightness_down" => {
                let _: EmptyArgs = serde_json::from_value(params)?;
                let step = i16::from(ctx.config.utilities.brightness_step_pct)
                    * if action.ends_with("_down") { -1 } else { 1 };
                self.brightness()?.adjust(step)?;
                let mut result = self.status();
                result["queued"] = json!(true);
                Ok(result)
            }
            _ => bail!("{}: {action}", tr("utilities.unknown_action")),
        }
    }
}
impl OrbmentModule for UtilitiesModule {
    fn id(&self) -> &'static str {
        "utilities"
    }
    fn service_state(&self) -> ServiceState {
        if self.running {
            ServiceState::Running
        } else {
            ServiceState::Disabled
        }
    }
    fn ui_state(&self) -> UiSurfaceState {
        if self.picker.as_ref().is_some_and(UtilityOverlay::is_alive)
            || self.borders.values().any(UtilityOverlay::is_alive)
        {
            UiSurfaceState::Visible
        } else {
            UiSurfaceState::Unloaded
        }
    }
    fn start_service(&mut self, ctx: &CoreContext) -> Result<()> {
        self.apply_config(ctx)
    }
    fn open_ui(&mut self, _trigger: ActivateTrigger, ctx: &CoreContext) -> Result<()> {
        self.command("utilities.pick_color", json!({}), ctx)?;
        Ok(())
    }
    fn close_ui(&mut self, _ctx: &CoreContext) -> Result<()> {
        self.picker = None;
        self.borders.clear();
        Ok(())
    }
    fn reclaim_idle_ui_and_cache(&mut self, _ctx: &CoreContext) -> Result<()> {
        self.maintenance()
    }
    fn stop_service(&mut self, ctx: &CoreContext) -> Result<()> {
        self.close_ui(ctx)?;
        self.awake.stop();
        self.brightness = None;
        self.running = false;
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TopmostArgs {
    hwnd: Option<isize>,
    title: Option<String>,
    target: Option<TopmostTarget>,
    enabled: Option<bool>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PickerArgs {
    format: Option<ColorFormat>,
    x: Option<i32>,
    y: Option<i32>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AwakeArgs {
    enabled: Option<bool>,
    mode: Option<KeepAwakeMode>,
    duration: Option<KeepAwakeDuration>,
    process_id: Option<u32>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BrightnessArgs {
    id: Option<u64>,
    percent: Option<u8>,
    delta: Option<i16>,
    #[serde(default)]
    refresh: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}
