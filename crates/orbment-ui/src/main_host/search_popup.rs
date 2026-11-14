use super::*;
use crate::{FileToolsAction, SharedFileToolsState, file_tools::SearchView};
use orbment_core::{config::FileSearchConfig, i18n::tr};

/// Search shares the host's IME, popup placement, focus-loss and graphics lifecycle.
pub struct SearchPopupWindow {
    graphics: RefCell<Rc<D2dContext>>,
    native: RefCell<Option<MainHostWindow>>,
    lifecycle: Rc<PopupLifecycle>,
    state: SharedFileToolsState,
    events: RefCell<Vec<SettingsEvent>>,
    wake_hwnd: HWND,
    revision: Cell<Option<u64>>,
}
impl SearchPopupWindow {
    pub fn new(
        graphics: Rc<D2dContext>,
        wake_hwnd: HWND,
        state: SharedFileToolsState,
        started: Instant,
    ) -> Self {
        let mut placement = ClipboardConfig::default();
        placement.window.position = "center".into();
        Self {
            graphics: RefCell::new(graphics),
            native: RefCell::new(None),
            state,
            events: RefCell::new(Vec::new()),
            wake_hwnd,
            revision: Cell::new(None),
            lifecycle: Rc::new(PopupLifecycle {
                config: RefCell::new(placement),
                events: RefCell::new(Vec::new()),
                hidden: Cell::new(Some(Instant::now())),
                last_active: Cell::new(HWND::default()),
                position: Cell::new(None),
                fixed: Cell::new(false),
                start: started,
                search: true,
                panel: false,
            }),
        }
    }
    pub fn show(&self, config: &FileSearchConfig) -> Result<()> {
        anyhow::ensure!(config.enabled, tr("file_tools.disabled"));
        if self.is_visible() {
            if let Some(native) = self.native.borrow().as_ref() {
                native.show();
            }
            return Ok(());
        }
        let foreground = unsafe { GetForegroundWindow() };
        self.lifecycle.last_active.set(foreground);
        if self
            .native
            .borrow()
            .as_ref()
            .is_none_or(|native| !native.is_alive())
        {
            self.unload();
            let native = MainHostWindow::create(
                self.graphics.borrow().clone(),
                ViewBody::Search(Box::new(SearchView::new(self.state.clone()))),
                self.wake_hwnd,
                Some(self.lifecycle.clone()),
            )?;
            *self.native.borrow_mut() = Some(native);
        } else if let Some(native) = self.native.borrow().as_ref()
            && let Some(view) = native.0.view.borrow_mut().as_mut()
        {
            view.body = ViewBody::Search(Box::new(SearchView::new(self.state.clone())));
        }
        let native = self.native.borrow();
        let native = native.as_ref().context("Search popup was not created")?;
        native.0.position_popup()?;
        self.lifecycle.hidden.set(None);
        native
            .0
            .emit(SettingsEvent::FileTools(FileToolsAction::SearchOpened));
        native.show();
        Ok(())
    }
    pub fn hide(&self) {
        if let Some(native) = self.native.borrow().as_ref() {
            native.0.hide_popup();
        }
    }
    pub fn is_visible(&self) -> bool {
        self.lifecycle.hidden.get().is_none()
            && self
                .native
                .borrow()
                .as_ref()
                .is_some_and(MainHostWindow::is_alive)
    }
    pub fn hwnd(&self) -> HWND {
        self.native
            .borrow()
            .as_ref()
            .map_or(HWND::default(), MainHostWindow::hwnd)
    }
    pub fn take_events(&self) -> Vec<SettingsEvent> {
        let mut events = std::mem::take(&mut *self.events.borrow_mut());
        if let Some(native) = self.native.borrow().as_ref() {
            events.extend(native.take_events());
        }
        events
    }
    pub fn hidden_since(&self) -> Option<Instant> {
        if self.hwnd().is_invalid() {
            None
        } else {
            self.lifecycle.hidden.get()
        }
    }
    pub fn idle_deadline_ms(&self, idle_seconds: u64) -> Option<u64> {
        self.hidden_since().map(|hidden| {
            (hidden.duration_since(self.lifecycle.start).as_millis() as u64)
                .saturating_add(idle_seconds.saturating_mul(1000))
        })
    }
    pub fn tick(&self, idle_seconds: u64) {
        if let Some(hidden) = self.hidden_since() {
            if hidden.elapsed() >= Duration::from_secs(idle_seconds) {
                self.unload();
            }
        } else if let Some(native) = self.native.borrow().as_ref() {
            let revision = self.state.lock().ok().map(|state| state.revision);
            if self.revision.replace(revision) != revision {
                native.0.request_repaint();
            }
        }
    }
    fn unload(&self) {
        let native = self.native.borrow_mut().take();
        if let Some(native) = &native {
            self.events.borrow_mut().extend(native.take_events());
        }
        drop(native);
        self.revision.set(None);
    }
    pub fn rebuild_graphics(&self, graphics: Rc<D2dContext>) -> Result<()> {
        if let Some(native) = self.native.borrow().as_ref() {
            native.rebuild_graphics(graphics.clone())?;
        }
        *self.graphics.borrow_mut() = graphics;
        Ok(())
    }
}
impl Drop for SearchPopupWindow {
    fn drop(&mut self) {
        self.hide();
    }
}
