use crate::config::OrbmentConfig;
use anyhow::Result;
use serde::Serialize;
use std::{cell::RefCell, collections::BTreeMap, path::PathBuf, rc::Rc};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ServiceState {
    Disabled,
    Running,
    Unavailable { reason: &'static str },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum UiSurfaceState {
    Unloaded,
    Visible,
    HiddenIdlePending { hidden_at_ms: u64 },
    HiddenResident,
}
#[derive(Debug, Clone, Copy)]
pub enum ActivateTrigger {
    Tray,
    Hotkey,
    Action,
}
pub struct CoreContext {
    pub config: OrbmentConfig,
    pub data_dir: PathBuf,
    pub ocr: std::sync::Arc<crate::ocr::OcrRouter>,
}
pub trait OrbmentModule {
    fn id(&self) -> &'static str;
    fn service_state(&self) -> ServiceState;
    fn ui_state(&self) -> UiSurfaceState;
    fn idle_timeout_seconds(&self, ctx: &CoreContext) -> u32 {
        ctx.config.app.idle_destroy_seconds
    }
    fn start_service(&mut self, ctx: &CoreContext) -> Result<()>;
    fn open_ui(&mut self, trigger: ActivateTrigger, ctx: &CoreContext) -> Result<()>;
    fn close_ui(&mut self, ctx: &CoreContext) -> Result<()>;
    fn reclaim_idle_ui_and_cache(&mut self, ctx: &CoreContext) -> Result<()>;
    fn stop_service(&mut self, ctx: &CoreContext) -> Result<()>;
}
#[derive(Default)]
pub struct ModuleManager {
    modules: BTreeMap<&'static str, Rc<RefCell<dyn OrbmentModule>>>,
}
impl ModuleManager {
    pub fn register(&mut self, module: Rc<RefCell<dyn OrbmentModule>>) {
        let id = module.borrow().id();
        self.modules.insert(id, module);
    }
    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::Value::Object(self.modules.iter().map(|(id, module)| {
            let module = module.borrow();
            ((*id).into(), serde_json::json!({"service_state": module.service_state(), "ui_state": module.ui_state()}))
        }).collect())
    }
    pub fn idle_deadline_ms(&self, ctx: &CoreContext) -> Option<u64> {
        self.modules
            .values()
            .filter_map(|module| {
                let module = module.borrow();
                match module.ui_state() {
                    UiSurfaceState::HiddenIdlePending { hidden_at_ms } => Some(
                        hidden_at_ms
                            .saturating_add(u64::from(module.idle_timeout_seconds(ctx)) * 1000),
                    ),
                    _ => None,
                }
            })
            .min()
    }
    pub fn reclaim_due(&mut self, ctx: &CoreContext, now_ms: u64) -> Result<()> {
        let mut first_error = None;
        for module in self.modules.values() {
            let state = module.borrow().ui_state();
            let timeout = module.borrow().idle_timeout_seconds(ctx);
            if let UiSurfaceState::HiddenIdlePending { hidden_at_ms } = state
                && now_ms.saturating_sub(hidden_at_ms) >= u64::from(timeout) * 1000
                && let Err(error) = module.borrow_mut().reclaim_idle_ui_and_cache(ctx)
            {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
    pub fn stop_all(&mut self, ctx: &CoreContext) -> Result<()> {
        let mut first_error = None;
        for module in self.modules.values().rev() {
            if let Err(error) = module.borrow_mut().stop_service(ctx) {
                first_error.get_or_insert(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, sync::Arc};

    struct IdleModule {
        id: &'static str,
        calls: Rc<Cell<u32>>,
        fail: bool,
    }

    impl OrbmentModule for IdleModule {
        fn id(&self) -> &'static str {
            self.id
        }
        fn service_state(&self) -> ServiceState {
            ServiceState::Running
        }
        fn ui_state(&self) -> UiSurfaceState {
            UiSurfaceState::HiddenIdlePending { hidden_at_ms: 100 }
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
        fn stop_service(&mut self, _: &CoreContext) -> Result<()> {
            Ok(())
        }
        fn reclaim_idle_ui_and_cache(&mut self, _: &CoreContext) -> Result<()> {
            self.calls.set(self.calls.get() + 1);
            anyhow::ensure!(!self.fail, "reclaim failed");
            Ok(())
        }
    }

    #[test]
    fn failed_reclaim_does_not_skip_other_due_modules() {
        let mut config = OrbmentConfig::default();
        config.app.idle_destroy_seconds = 1;
        let ctx = CoreContext {
            config,
            data_dir: PathBuf::new(),
            ocr: Arc::new(crate::ocr::OcrRouter::default()),
        };
        let first = Rc::new(Cell::new(0));
        let second = Rc::new(Cell::new(0));
        let mut manager = ModuleManager::default();
        for (id, calls, fail) in [("a", first.clone(), true), ("b", second.clone(), false)] {
            manager.register(Rc::new(RefCell::new(IdleModule { id, calls, fail })));
        }
        assert_eq!(manager.idle_deadline_ms(&ctx), Some(1100));
        assert!(manager.reclaim_due(&ctx, 1099).is_ok());
        assert_eq!((first.get(), second.get()), (0, 0));
        assert!(manager.reclaim_due(&ctx, 1100).is_err());
        assert_eq!((first.get(), second.get()), (1, 1));
    }
}
