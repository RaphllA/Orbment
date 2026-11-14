use super::*;

/// Application-owned contents; native input, IME, focus and rendering stay in the host.
pub trait PopupPanel {
    fn title(&self) -> String;
    fn ui(&mut self, context: &egui::Context);
}

pub struct TransientPanelWindow {
    graphics: RefCell<Rc<D2dContext>>,
    native: RefCell<Option<MainHostWindow>>,
    lifecycle: Rc<PopupLifecycle>,
    panel: Rc<RefCell<Box<dyn PopupPanel>>>,
    wake_hwnd: HWND,
}

impl TransientPanelWindow {
    pub fn new(
        graphics: Rc<D2dContext>,
        wake_hwnd: HWND,
        panel: Box<dyn PopupPanel>,
        started: Instant,
    ) -> Self {
        let mut placement = ClipboardConfig::default();
        placement.window.position = "followCursor".into();
        Self {
            graphics: RefCell::new(graphics),
            native: RefCell::new(None),
            panel: Rc::new(RefCell::new(panel)),
            wake_hwnd,
            lifecycle: Rc::new(PopupLifecycle {
                config: RefCell::new(placement),
                events: RefCell::new(Vec::new()),
                hidden: Cell::new(Some(Instant::now())),
                last_active: Cell::new(HWND::default()),
                position: Cell::new(None),
                fixed: Cell::new(false),
                start: started,
                search: false,
                panel: true,
            }),
        }
    }

    pub fn show(&self) -> Result<()> {
        let foreground = unsafe { GetForegroundWindow() };
        if !foreground.is_invalid() && foreground != self.hwnd() {
            self.lifecycle.last_active.set(foreground);
        }
        if self
            .native
            .borrow()
            .as_ref()
            .is_none_or(|native| !native.is_alive())
        {
            self.native.borrow_mut().take();
            let native = MainHostWindow::create(
                self.graphics.borrow().clone(),
                ViewBody::Panel(self.panel.clone()),
                self.wake_hwnd,
                Some(self.lifecycle.clone()),
            )?;
            *self.native.borrow_mut() = Some(native);
        }
        if let Some(native) = self.native.borrow().as_ref() {
            native.0.position_popup()?;
            self.lifecycle.hidden.set(None);
            native.show();
        }
        Ok(())
    }

    pub fn hide(&self) {
        if let Some(native) = self.native.borrow().as_ref() {
            native.0.hide_popup();
        }
    }

    pub fn restore_last_active(&self) -> bool {
        self.hide();
        popup::restore_foreground(self.lifecycle.last_active.get())
    }

    pub fn hwnd(&self) -> HWND {
        self.native
            .borrow()
            .as_ref()
            .map_or(HWND::default(), MainHostWindow::hwnd)
    }

    pub fn is_visible(&self) -> bool {
        !self.hwnd().is_invalid() && self.lifecycle.hidden.get().is_none()
    }

    pub fn set_dismiss_on_blur(&self, dismiss: bool) {
        self.lifecycle.fixed.set(!dismiss);
    }
    pub fn hidden_since(&self) -> Option<Instant> {
        if self.hwnd().is_invalid() {
            None
        } else {
            self.lifecycle.hidden.get()
        }
    }

    pub fn ui_state(&self) -> UiSurfaceState {
        if self.hwnd().is_invalid() {
            UiSurfaceState::Unloaded
        } else if let Some(hidden) = self.hidden_since() {
            UiSurfaceState::HiddenIdlePending {
                hidden_at_ms: hidden.duration_since(self.lifecycle.start).as_millis() as u64,
            }
        } else {
            UiSurfaceState::Visible
        }
    }

    pub fn request_repaint(&self) {
        if let Some(native) = self.native.borrow().as_ref() {
            native.0.request_repaint();
        }
    }

    pub fn take_events(&self) -> Vec<SettingsEvent> {
        self.native
            .borrow()
            .as_ref()
            .map_or_else(Vec::new, MainHostWindow::take_events)
    }

    pub fn unload(&self) {
        self.hide();
        let native = self.native.borrow_mut().take();
        drop(native);
    }

    pub fn rebuild_graphics(&self, graphics: Rc<D2dContext>) -> Result<()> {
        if let Some(native) = self.native.borrow().as_ref() {
            native.rebuild_graphics(graphics.clone())?;
        }
        *self.graphics.borrow_mut() = graphics;
        Ok(())
    }
}
