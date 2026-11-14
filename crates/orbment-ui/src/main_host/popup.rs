use super::*;
use anyhow::Context as _;

/// Lazily owns a popup HWND, egui context, swapchain and bounded thumbnail cache.
pub struct TransientPopupWindow {
    graphics: RefCell<Rc<D2dContext>>,
    native: RefCell<Option<MainHostWindow>>,
    lifecycle: Rc<PopupLifecycle>,
    wake_hwnd: HWND,
}
impl TransientPopupWindow {
    pub fn new(graphics: Rc<D2dContext>, wake_hwnd: HWND, started: Instant) -> Self {
        Self {
            graphics: RefCell::new(graphics),
            native: RefCell::new(None),
            wake_hwnd,
            lifecycle: Rc::new(PopupLifecycle {
                config: RefCell::new(ClipboardConfig::default()),
                events: RefCell::new(Vec::new()),
                hidden: Cell::new(None),
                last_active: Cell::new(HWND::default()),
                position: Cell::new(None),
                fixed: Cell::new(false),
                start: started,
                search: false,
                panel: false,
            }),
        }
    }
    pub fn show(&self, config: &ClipboardConfig) -> Result<()> {
        let foreground = unsafe { GetForegroundWindow() };
        if external_window(foreground) {
            self.lifecycle.last_active.set(foreground);
        }
        *self.lifecycle.config.borrow_mut() = config.clone();
        if self
            .native
            .borrow()
            .as_ref()
            .is_none_or(|native| !native.is_alive())
        {
            self.native.borrow_mut().take();
            let native = MainHostWindow::create(
                self.graphics.borrow().clone(),
                ViewBody::Clipboard(Box::new(ClipboardView::new(config.clone()))),
                self.wake_hwnd,
                Some(self.lifecycle.clone()),
            )?;
            *self.native.borrow_mut() = Some(native);
        }
        let native = self.native.borrow();
        let native = native.as_ref().context("Popup window was not created")?;
        self.lifecycle.fixed.set(false);
        if let Some(view) = native.0.view.borrow_mut().as_mut()
            && let ViewBody::Clipboard(view) = &mut view.body
        {
            view.opened(config);
        }
        native.0.position_popup()?;
        self.lifecycle.hidden.set(None);
        native.show();
        Ok(())
    }
    pub fn hwnd(&self) -> HWND {
        self.native
            .borrow()
            .as_ref()
            .map_or(HWND::default(), MainHostWindow::hwnd)
    }
    pub fn hide(&self) {
        if let Some(native) = self.native.borrow().as_ref() {
            native.0.hide_popup();
        }
    }
    pub fn restore_last_active(&self) -> Option<HWND> {
        let visible = matches!(self.ui_state(), UiSurfaceState::Visible);
        let foreground = unsafe { GetForegroundWindow() };
        let target = if !visible && external_window(foreground) {
            foreground
        } else {
            self.lifecycle.last_active.get()
        };
        self.hide();
        if external_window(target) && restore_foreground(target) {
            Some(target)
        } else {
            self.lifecycle
                .events
                .borrow_mut()
                .push(ClipboardPopupEvent::Error(orbment_core::i18n::tr(
                    "clipboard.target_unavailable",
                )));
            crate::wake(self.wake_hwnd);
            None
        }
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
            return UiSurfaceState::Unloaded;
        }
        match self.lifecycle.hidden.get() {
            None => UiSurfaceState::Visible,
            Some(hidden) if self.lifecycle.config.borrow().window.lightweight_mode => {
                UiSurfaceState::HiddenIdlePending {
                    hidden_at_ms: hidden.duration_since(self.lifecycle.start).as_millis() as u64,
                }
            }
            Some(_) => UiSurfaceState::HiddenResident,
        }
    }
    pub fn take_events(&self) -> Vec<ClipboardPopupEvent> {
        std::mem::take(&mut *self.lifecycle.events.borrow_mut())
    }
    pub fn reclaim(&self, idle_seconds: u64) -> bool {
        let due = self.lifecycle.config.borrow().window.lightweight_mode
            && self
                .hidden_since()
                .is_some_and(|hidden| hidden.elapsed() >= Duration::from_secs(idle_seconds));
        if due {
            self.unload();
        }
        due
    }
    pub fn unload(&self) {
        // DestroyWindow synchronously enters the WndProc, outside this RefCell borrow.
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
    pub fn sync_config(&self, config: &ClipboardConfig) {
        *self.lifecycle.config.borrow_mut() = config.clone();
        self.with_view(|view| view.config = config.clone());
    }
    pub fn set_page(&self, generation: u64, total: usize, offset: usize, rows: Vec<ClipboardRow>) {
        self.with_view(|view| view.set_page(generation, total, offset, rows));
    }
    pub fn set_groups(&self, groups: Vec<ClipboardGroup>) {
        self.with_view(|view| view.set_groups(groups));
    }
    pub fn refresh(&self) {
        self.with_view(ClipboardView::refresh);
    }
    fn with_view(&self, update: impl FnOnce(&mut ClipboardView)) {
        if let Some(native) = self.native.borrow().as_ref() {
            if let Some(view) = native.0.view.borrow_mut().as_mut()
                && let ViewBody::Clipboard(view) = &mut view.body
            {
                update(view);
            }
            native.0.request_repaint();
        }
    }
}
fn external_window(window: HWND) -> bool {
    unsafe {
        if window.is_invalid() || !IsWindow(Some(window)).as_bool() {
            return false;
        }
        let mut pid = 0;
        GetWindowThreadProcessId(window, Some(&mut pid));
        pid != 0 && pid != std::process::id()
    }
}

pub(super) fn restore_foreground(target: HWND) -> bool {
    unsafe {
        if target.is_invalid() || !IsWindow(Some(target)).as_bool() {
            return false;
        }
        if IsIconic(target).as_bool() {
            let _ = ShowWindow(target, SW_RESTORE);
        }
        let _ = SetForegroundWindow(target);
        GetForegroundWindow() == target
    }
}
impl Host {
    pub(super) fn emit_popup(&self, event: ClipboardPopupEvent) {
        if let Some(popup) = &self.popup {
            popup.events.borrow_mut().push(event);
            crate::wake(self.wake_hwnd);
        }
    }
    pub(super) fn hide_popup(&self) {
        let Some(popup) = &self.popup else {
            return;
        };
        if self.hwnd.get().is_invalid() || popup.hidden.get().is_some() {
            return;
        }
        popup.hidden.set(Some(Instant::now()));
        self.ime.finish(self.hwnd.get());
        let mut events = Vec::new();
        let mut commands = Vec::new();
        {
            let mut input = self.input.borrow_mut().take();
            input.time = Some(self.start.elapsed().as_secs_f64());
            input
                .events
                .push(Event::Ime(ImeEvent::Preedit(String::new())));
            input.events.push(Event::Ime(ImeEvent::Disabled));
            if let Some(view) = self.view.borrow_mut().as_mut() {
                let output = view.context.run(input, |context| match &mut view.body {
                    ViewBody::Settings(settings) => settings.draw(context, &mut events),
                    ViewBody::Clipboard(clipboard) => commands = clipboard.draw(context),
                    ViewBody::Search(search) => {
                        search.draw(context, &mut events);
                    }
                    ViewBody::Panel(panel) => panel.borrow_mut().ui(context),
                });
                for (id, delta) in &output.textures_delta.set {
                    if let Err(error) = view.renderer.update(*id, delta) {
                        events.push(SettingsEvent::Error(error.to_string()));
                        break;
                    }
                }
                view.renderer.free(&output.textures_delta.free);
                if let ViewBody::Clipboard(clipboard) = &mut view.body {
                    clipboard.hidden();
                }
            }
        }
        unsafe {
            let _ = KillTimer(Some(self.hwnd.get()), REPAINT_TIMER);
            let _ = ShowWindow(self.hwnd.get(), SW_HIDE);
        }
        self.deadline.set(None);
        *self.input.borrow_mut() = egui::RawInput::default();
        // Dispatch only after releasing view/input borrows and completing the native hide.
        for event in events {
            self.emit(event);
        }
        self.popup_commands(commands);
        if popup.search {
            self.emit(SettingsEvent::FileTools(
                crate::FileToolsAction::SearchClosed,
            ));
        } else if !popup.panel {
            self.emit_popup(ClipboardPopupEvent::Hidden);
        } else {
            crate::wake(self.wake_hwnd);
        }
    }
    pub(super) fn position_popup(&self) -> Result<()> {
        let Some(popup) = &self.popup else {
            return Ok(());
        };
        let config = popup.config.borrow();
        unsafe {
            let mut cursor = POINT::default();
            GetCursorPos(&mut cursor)?;
            let anchor = popup_anchor(&config, popup.position.get(), cursor);
            let monitor = MonitorFromPoint(anchor, MONITOR_DEFAULTTONEAREST);
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            ensure!(
                GetMonitorInfoW(monitor, &mut info).as_bool(),
                "GetMonitorInfoW failed"
            );
            let area = info.rcWork;
            // Move first so the destination monitor supplies the popup DPI.
            SetWindowPos(
                self.hwnd.get(),
                None,
                anchor.x,
                anchor.y,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            )?;
            let scale = GetDpiForWindow(self.hwnd.get()).max(96) as f32 / 96.0;
            let width = ((420.0 * scale) as i32).min(area.right - area.left).max(1);
            let height = ((620.0 * scale) as i32).min(area.bottom - area.top).max(1);
            let (mut x, mut y) = match config.window.position.as_str() {
                "center" => (
                    area.left + (area.right - area.left - width) / 2,
                    area.top + (area.bottom - area.top - height) / 2,
                ),
                "dock" => match config.window.dock_edge.as_str() {
                    "left" => (area.left, area.top),
                    "top" => (area.left + (area.right - area.left - width) / 2, area.top),
                    "bottom" => (
                        area.left + (area.right - area.left - width) / 2,
                        area.bottom - height,
                    ),
                    _ => (area.right - width, area.top),
                },
                _ => (anchor.x, anchor.y),
            };
            x = x.clamp(area.left, area.right - width);
            y = y.clamp(area.top, area.bottom - height);
            SetWindowPos(
                self.hwnd.get(),
                Some(HWND_TOPMOST),
                x,
                y,
                width,
                height,
                SWP_NOACTIVATE,
            )?;
        }
        Ok(())
    }
    pub(super) fn popup_commands(&self, commands: Vec<PopupCommand>) {
        for command in commands {
            match command {
                PopupCommand::Event(event) => self.emit_popup(event),
                PopupCommand::Hide => self.hide_popup(),
                PopupCommand::Fixed(fixed) => {
                    if let Some(popup) = &self.popup {
                        popup.fixed.set(fixed);
                    }
                }
                PopupCommand::Preview(open) => unsafe {
                    let mut rect = RECT::default();
                    if GetWindowRect(self.hwnd.get(), &mut rect).is_ok() {
                        let scale = GetDpiForWindow(self.hwnd.get()).max(96) as f32 / 96.0;
                        let monitor = MonitorFromPoint(
                            POINT {
                                x: rect.left,
                                y: rect.top,
                            },
                            MONITOR_DEFAULTTONEAREST,
                        );
                        let mut info = MONITORINFO {
                            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                            ..Default::default()
                        };
                        if GetMonitorInfoW(monitor, &mut info).as_bool() {
                            let width = (((if open { 720.0 } else { 420.0 }) * scale) as i32)
                                .min(info.rcWork.right - info.rcWork.left);
                            let _ = SetWindowPos(
                                self.hwnd.get(),
                                None,
                                rect.left.clamp(info.rcWork.left, info.rcWork.right - width),
                                rect.top,
                                width,
                                rect.bottom - rect.top,
                                SWP_NOACTIVATE | SWP_NOZORDER,
                            );
                        }
                    }
                },
                PopupCommand::FileDrag(paths) => {
                    // OLE pumps messages; run after releasing the view borrow and keep the source alive.
                    let previous = self.popup.as_ref().map(|popup| popup.fixed.replace(true));
                    unsafe {
                        let _ = ReleaseCapture();
                    }
                    let result = crate::ui_popup::start_file_drag(&paths);
                    if let (Some(popup), Some(previous)) = (&self.popup, previous) {
                        popup.fixed.set(previous);
                    }
                    let position = self
                        .view
                        .borrow()
                        .as_ref()
                        .and_then(|view| view.context.input(|input| input.pointer.latest_pos()))
                        .unwrap_or_default();
                    let mut input = self.input.borrow_mut();
                    // OLE consumes the release message; explicitly end egui's drag too.
                    input.events.push(Event::PointerButton {
                        pos: position,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: Modifiers::NONE,
                    });
                    input.events.push(Event::PointerGone);
                    drop(input);
                    if let Err(error) = result {
                        self.emit_popup(ClipboardPopupEvent::Error(error.to_string()));
                    }
                    self.request_repaint();
                }
                PopupCommand::Drag => unsafe {
                    let _ = ReleaseCapture();
                    let _ = PostMessageW(
                        Some(self.hwnd.get()),
                        WM_NCLBUTTONDOWN,
                        WPARAM(HTCAPTION as usize),
                        LPARAM(0),
                    );
                },
                PopupCommand::Paste { id, plain, as_path } => {
                    self.hide_popup();
                    if let Some(popup) = &self.popup {
                        let target = popup.last_active.get();
                        if restore_foreground(target) {
                            self.emit_popup(ClipboardPopupEvent::Paste {
                                id,
                                plain,
                                as_path,
                                target,
                            });
                        } else {
                            self.emit_popup(ClipboardPopupEvent::Error(orbment_core::i18n::tr(
                                "clipboard.target_unavailable",
                            )));
                        }
                    }
                }
            }
        }
    }
}

fn popup_anchor(config: &ClipboardConfig, remembered: Option<POINT>, cursor: POINT) -> POINT {
    match config.window.position.as_str() {
        "fixed" => POINT {
            x: config.window.pos_x,
            y: config.window.pos_y,
        },
        "remember" => remembered.unwrap_or(cursor),
        _ => cursor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hiding_applies_texture_updates_and_frees_and_reports_update_errors() {
        let graphics = Rc::new(D2dContext::new().unwrap());
        let popup = TransientPopupWindow::new(graphics.clone(), HWND::default(), Instant::now());
        let native = MainHostWindow::build(
            graphics,
            ViewBody::Clipboard(Box::new(ClipboardView::new(ClipboardConfig::default()))),
            HWND::default(),
            Some(popup.lifecycle.clone()),
        )
        .unwrap();
        let context = native.0.view.borrow().as_ref().unwrap().context.clone();
        let image = egui::ColorImage::new([1, 1], egui::Color32::WHITE);
        let options = egui::TextureOptions::LINEAR;
        let freed = context.load_texture("freed-on-hide", image.clone(), options);
        {
            let output = context.run(egui::RawInput::default(), |_| {});
            let mut view = native.0.view.borrow_mut();
            let view = view.as_mut().unwrap();
            for (id, delta) in &output.textures_delta.set {
                view.renderer.update(*id, delta).unwrap();
            }
            view.renderer.free(&output.textures_delta.free);
        }
        let mut retained = context.load_texture("created-on-hide", image.clone(), options);
        let freed_id = freed.id();
        drop(freed);

        native.0.hide_popup();

        let events = popup.take_events();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ClipboardPopupEvent::Hidden))
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ClipboardPopupEvent::Error(_)))
        );
        {
            let mut view = native.0.view.borrow_mut();
            let view = view.as_mut().unwrap();
            let patch = egui::epaint::ImageDelta::partial([0, 0], image.clone(), options);
            view.renderer.update(retained.id(), &patch).unwrap();
            assert!(view.renderer.update(freed_id, &patch).is_err());
            view.renderer.free(&[retained.id()]);
        }

        // A lost base image must surface through the same event channel as a paint error.
        retained.set_partial([0, 0], image, options);
        popup.lifecycle.hidden.set(None);
        native.0.hide_popup();
        assert!(popup.take_events().iter().any(|event| matches!(
            event,
            ClipboardPopupEvent::Error(error) if error.contains("Texture patch has no base image")
        )));
        assert!(popup.lifecycle.hidden.get().is_some());
    }

    #[test]
    fn cursor_position_modes_ignore_the_previous_popup_location() {
        let mut config = ClipboardConfig::default();
        let remembered = POINT { x: 10, y: 20 };
        for mode in ["followCursor", "center", "dock", "unknown"] {
            config.window.position = mode.into();
            for cursor in [POINT { x: 100, y: 200 }, POINT { x: -500, y: 50 }] {
                assert_eq!(popup_anchor(&config, Some(remembered), cursor), cursor);
            }
        }
        config.window.position = "remember".into();
        let cursor = POINT { x: 100, y: 200 };
        assert_eq!(popup_anchor(&config, Some(remembered), cursor), remembered);
        assert_eq!(popup_anchor(&config, None, cursor), cursor);
        config.window.position = "fixed".into();
        config.window.pos_x = -1000;
        config.window.pos_y = 40;
        assert_eq!(
            popup_anchor(&config, Some(remembered), cursor),
            POINT { x: -1000, y: 40 },
        );
    }
}
