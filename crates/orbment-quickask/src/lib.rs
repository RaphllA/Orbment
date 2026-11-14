pub mod context_capture;
pub mod llm_client;
pub mod ui_popup;

pub use context_capture::{ContextImage, QuickContext};
pub use llm_client::{PendingToolCall, fetch_models, read_api_key};
pub use ui_popup::{QuickAskState, QuickAskView, SharedQuickAskState};

use anyhow::{Result, ensure};
use llm_client::{Conversation, LlmEvent, LlmJob};
use orbment_core::{
    action_bus::ActionBus,
    config::{McpConfig, QuickAskConfig},
    i18n::tr,
    lifecycle::UiSurfaceState,
};
use orbment_ui::{D2dContext, SettingsEvent, TransientPanelWindow};
use std::{
    rc::Rc,
    sync::{Arc, Mutex, mpsc},
    time::Instant,
};
use ui_popup::{ChatLine, ChatRole, QuickAskEvent};
use windows::Win32::{Foundation::HWND, UI::WindowsAndMessaging::GetForegroundWindow};

pub struct QuickAskController {
    config: QuickAskConfig,
    popup: TransientPanelWindow,
    state: SharedQuickAskState,
    wake: Arc<dyn Fn() + Send + Sync>,
    capture: Option<mpsc::Receiver<Result<QuickContext>>>,
    events: Option<mpsc::Receiver<LlmEvent>>,
    job: Option<LlmJob>,
    conversation: Conversation,
    tools: Vec<PendingToolCall>,
    device_lost: bool,
}
impl QuickAskController {
    pub fn new(graphics: Rc<D2dContext>, wake_hwnd: HWND, started: Instant) -> Self {
        let state = Arc::new(Mutex::new(QuickAskState::default()));
        let address = wake_hwnd.0 as isize;
        Self {
            popup: TransientPanelWindow::new(
                graphics,
                wake_hwnd,
                Box::new(QuickAskView::new(state.clone(), wake_hwnd)),
                started,
            ),
            config: QuickAskConfig::default(),
            state,
            wake: Arc::new(move || orbment_ui::wake(HWND(address as *mut _))),
            capture: None,
            events: None,
            job: None,
            conversation: Conversation::default(),
            tools: Vec::new(),
            device_lost: false,
        }
    }
    pub fn configure(&mut self, config: QuickAskConfig) -> Result<()> {
        config.validate()?;
        let changed = self.config.active_channel != config.active_channel
            || self.config.provider != config.provider
            || self.config.base_url != config.base_url
            || self.config.model != config.model
            || self.config.credential_target != config.credential_target
            || self.config.system_prompt != config.system_prompt
            || self.config.thinking_effort != config.thinking_effort;
        if changed {
            self.cancel();
            self.conversation = Conversation::default();
            self.state
                .lock()
                .expect("QuickAsk state poisoned")
                .messages
                .clear();
        }
        {
            let mut state = self.state.lock().expect("QuickAsk state poisoned");
            state.prompts = config.prompts.clone();
            state.channel_name = config.active_channel_config().name.clone();
            state.provider = config.provider;
            state.model = config.model.clone();
            state.thinking_effort = config.thinking_effort;
        }
        if !config.enabled {
            self.shutdown();
        }
        self.config = config;
        self.popup.request_repaint();
        Ok(())
    }
    pub fn show_selection(&mut self) -> Result<()> {
        ensure!(self.config.enabled, "{}", tr("quick_ask.disabled"));
        self.cancel();
        let foreground = unsafe { GetForegroundWindow() }.0 as isize;
        let (send, receive) = mpsc::channel();
        let wake = self.wake.clone();
        std::thread::Builder::new()
            .name("orbment-quickask-selection".into())
            .spawn(move || {
                let _ = send.send(context_capture::capture_selected_text(foreground));
                wake();
            })?;
        self.capture = Some(receive);
        Ok(())
    }
    pub fn show_context(&mut self, context: QuickContext) -> Result<()> {
        ensure!(self.config.enabled, "{}", tr("quick_ask.disabled"));
        context.validate()?;
        self.cancel();
        self.capture = None;
        self.conversation = Conversation::default();
        {
            let mut state = self.state.lock().expect("QuickAsk state poisoned");
            state.context = context;
            state.messages.clear();
            state.prompt.clear();
            state.error = None;
            state.events.clear();
        }
        self.popup.show()?;
        self.popup.request_repaint();
        Ok(())
    }
    pub fn hide(&self) {
        self.popup.hide();
    }
    pub fn ui_state(&self) -> UiSurfaceState {
        self.popup.ui_state()
    }
    pub fn tick(&mut self, bus: &ActionBus, policy: &McpConfig) -> Result<()> {
        let window_result = process_window_events(self.popup.take_events(), &mut self.device_lost);
        if self.device_lost {
            (self.wake)();
        }
        window_result?;
        if let Some(receiver) = self.capture.take() {
            match receiver.try_recv() {
                Ok(result) => self.show_context(result?)?,
                Err(mpsc::TryRecvError::Empty) => self.capture = Some(receiver),
                Err(mpsc::TryRecvError::Disconnected) => {
                    anyhow::bail!("{}", tr("quick_ask.capture_failed"))
                }
            }
        }
        let events: Vec<_> = self
            .events
            .as_ref()
            .map(|receiver| receiver.try_iter().collect())
            .unwrap_or_default();
        let mut changed = !events.is_empty();
        for event in events {
            match event {
                LlmEvent::Text(text) => {
                    self.conversation.push_text(&text);
                    let mut state = self.state.lock().expect("QuickAsk state poisoned");
                    if let Some(message) = state
                        .messages
                        .last_mut()
                        .filter(|message| matches!(message.role, ChatRole::Assistant))
                    {
                        message.text.push_str(&text);
                    } else {
                        state.messages.push(ChatLine {
                            role: ChatRole::Assistant,
                            text,
                        });
                    }
                }
                LlmEvent::Tool(call) => {
                    self.state
                        .lock()
                        .expect("QuickAsk state poisoned")
                        .messages
                        .push(ChatLine {
                            role: ChatRole::Tool,
                            text: call.call.name.clone(),
                        });
                    self.tools.push(call);
                }
                LlmEvent::Finished(conversation) => {
                    self.conversation = conversation;
                    self.job = None;
                    self.events = None;
                    self.state.lock().expect("QuickAsk state poisoned").busy = false;
                }
                LlmEvent::Failed(error) => {
                    self.conversation.fail_turn();
                    self.cancel();
                    self.state.lock().expect("QuickAsk state poisoned").error = Some(error);
                }
            }
        }
        let actions: Vec<_> = self
            .state
            .lock()
            .expect("QuickAsk state poisoned")
            .events
            .drain(..)
            .collect();
        changed |= !actions.is_empty();
        for action in actions {
            match action {
                QuickAskEvent::Cancel => self.cancel(),
                QuickAskEvent::Clear => {
                    self.cancel();
                    self.conversation = Conversation::default();
                    let mut state = self.state.lock().expect("QuickAsk state poisoned");
                    state.messages.clear();
                    state.error = None;
                }
                QuickAskEvent::Submit(prompt) => {
                    if self.job.is_none() && self.config.enabled && !prompt.trim().is_empty() {
                        let context = self
                            .state
                            .lock()
                            .expect("QuickAsk state poisoned")
                            .context
                            .clone();
                        let (send, receive) = mpsc::channel();
                        match llm_client::start_turn(
                            self.config.clone(),
                            context,
                            prompt.clone(),
                            self.conversation.clone(),
                            llm_client::available_tools(bus, policy),
                            send,
                            self.wake.clone(),
                        ) {
                            Ok((job, conversation)) => {
                                self.conversation = conversation;
                                self.job = Some(job);
                                self.events = Some(receive);
                                let mut state = self.state.lock().expect("QuickAsk state poisoned");
                                state.busy = true;
                                state.error = None;
                                state.messages.push(ChatLine {
                                    role: ChatRole::User,
                                    text: prompt,
                                });
                            }
                            Err(error) => {
                                self.state.lock().expect("QuickAsk state poisoned").error =
                                    Some(format!("{error:#}"))
                            }
                        }
                    }
                }
            }
        }
        if changed {
            self.popup.request_repaint();
        }
        Ok(())
    }
    pub fn take_device_lost(&mut self) -> bool {
        std::mem::take(&mut self.device_lost)
    }
    pub fn take_tool_calls(&mut self) -> Vec<PendingToolCall> {
        std::mem::take(&mut self.tools)
    }
    pub fn rebuild_graphics(&self, graphics: Rc<D2dContext>) -> Result<()> {
        self.popup.rebuild_graphics(graphics)
    }
    pub fn shutdown(&mut self) {
        self.cancel();
        self.capture = None;
        self.popup.hide();
        self.popup.unload();
        self.device_lost = false;
        self.conversation = Conversation::default();
        let mut state = self.state.lock().expect("QuickAsk state poisoned");
        state.context = QuickContext::default();
        state.messages.clear();
        state.prompt.clear();
        state.error = None;
        state.events.clear();
    }
    fn cancel(&mut self) {
        self.conversation.cancel_turn();
        self.job = None;
        self.events = None;
        self.tools.clear();
        self.state.lock().expect("QuickAsk state poisoned").busy = false;
    }
}
impl Drop for QuickAskController {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn process_window_events(
    events: impl IntoIterator<Item = SettingsEvent>,
    device_lost: &mut bool,
) -> Result<()> {
    let mut failure = None;
    for event in events {
        match event {
            SettingsEvent::DeviceLost => *device_lost = true,
            SettingsEvent::Error(error) => {
                failure.get_or_insert(error);
            }
            _ => {}
        }
    }
    failure.map_or(Ok(()), |error| Err(anyhow::anyhow!(error)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_errors_do_not_discard_device_loss_later_in_the_batch() {
        let mut device_lost = false;
        let mut consumed = 0;
        let result = process_window_events(
            [
                SettingsEvent::Error("first render error".into()),
                SettingsEvent::DeviceLost,
                SettingsEvent::Error("later render error".into()),
            ]
            .into_iter()
            .inspect(|_| consumed += 1),
            &mut device_lost,
        );
        assert_eq!(result.unwrap_err().to_string(), "first render error");
        assert!(device_lost);
        assert_eq!(consumed, 3);
        process_window_events([], &mut device_lost).unwrap();
        assert!(device_lost);
    }
}
