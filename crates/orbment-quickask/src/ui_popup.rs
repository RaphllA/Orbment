use crate::context_capture::QuickContext;
use orbment_core::{
    config::{QuickAskPrompt, QuickAskProvider, ThinkingEffort},
    i18n::{quick_ask_provider_label, thinking_effort_label, tr},
};
use orbment_ui::{
    PopupPanel,
    theme::{self, IconKind},
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use windows::Win32::Foundation::HWND;

#[derive(Clone, Copy, Debug)]
pub enum ChatRole {
    User,
    Assistant,
    Tool,
}
#[derive(Clone, Debug)]
pub struct ChatLine {
    pub role: ChatRole,
    pub text: String,
}
pub enum QuickAskEvent {
    Submit(String),
    Cancel,
    Clear,
}
#[derive(Default)]
pub struct QuickAskState {
    pub context: QuickContext,
    pub messages: Vec<ChatLine>,
    pub prompt: String,
    pub prompts: Vec<QuickAskPrompt>,
    pub channel_name: String,
    pub provider: QuickAskProvider,
    pub model: String,
    pub thinking_effort: ThinkingEffort,
    pub busy: bool,
    pub error: Option<String>,
    pub events: VecDeque<QuickAskEvent>,
}
pub type SharedQuickAskState = Arc<Mutex<QuickAskState>>;

pub struct QuickAskView {
    pub state: SharedQuickAskState,
    focus_prompt: bool,
    wake_hwnd: HWND,
}
impl QuickAskView {
    pub fn new(state: SharedQuickAskState, wake_hwnd: HWND) -> Self {
        Self {
            state,
            focus_prompt: true,
            wake_hwnd,
        }
    }
}
impl PopupPanel for QuickAskView {
    fn title(&self) -> String {
        tr("quick_ask.title")
    }
    fn ui(&mut self, context: &egui::Context) {
        let mut state = self.state.lock().expect("QuickAsk state poisoned");
        egui::TopBottomPanel::top("quickask-context")
            .show_separator_line(false)
            .frame(
                egui::Frame::new()
                    .fill(theme::NAV_BG)
                    .inner_margin(egui::Margin::same(12)),
            )
            .show(context, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    theme::icon(ui, IconKind::Chat, theme::ACCENT_LIGHT);
                    ui.label(theme::strong(ui, tr("quick_ask.title"), 15.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        theme::pill(
                            ui,
                            channel_badge(
                                &state.channel_name,
                                state.provider,
                                &state.model,
                                state.thinking_effort,
                            ),
                            theme::TEXT_MUTED,
                            theme::CARD,
                        );
                        if state.busy {
                            theme::pill(
                                ui,
                                tr("quick_ask.generating"),
                                theme::ACCENT_LIGHT,
                                theme::ACCENT_SOFT,
                            );
                            ui.spinner();
                        }
                    });
                });
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
                    if !state.context.source_title.is_empty() {
                        theme::icon_pill(
                            ui,
                            IconKind::Monitor,
                            format!("{}: {}", tr("quick_ask.source"), state.context.source_title),
                            theme::TEXT_MUTED,
                            theme::CARD,
                        );
                    }
                    if let Some(image) = &state.context.image {
                        theme::icon_pill(
                            ui,
                            IconKind::Image,
                            format!(
                                "{} · {} · {} KiB",
                                tr("quick_ask.image_attached"),
                                image.mime_type,
                                image.bytes.len().div_ceil(1024)
                            ),
                            theme::GREEN,
                            theme::GREEN_BG,
                        );
                    }
                    if !state.context.text.is_empty() {
                        let mut chars = state.context.text.chars();
                        let summary: String = chars.by_ref().take(28).collect();
                        theme::icon_pill(
                            ui,
                            IconKind::Note,
                            format!(
                                "{} · {}{}",
                                tr("quick_ask.context"),
                                summary.replace('\n', " "),
                                if chars.next().is_some() { "…" } else { "" }
                            ),
                            theme::ACCENT_LIGHT,
                            theme::ACCENT_SOFT,
                        );
                    }
                });
                if !state.context.text.is_empty() {
                    egui::CollapsingHeader::new(theme::muted(tr("quick_ask.show_context"))).show(
                        ui,
                        |ui| {
                            egui::ScrollArea::vertical()
                                .max_height(100.0)
                                .show(ui, |ui| {
                                    ui.add(
                                        egui::Label::new(theme::muted(state.context.text.as_str()))
                                            .selectable(true),
                                    );
                                });
                        },
                    );
                }
            });
        egui::TopBottomPanel::bottom("quickask-input")
            .show_separator_line(false)
            .frame(
                egui::Frame::new()
                    .fill(theme::NAV_BG)
                    .inner_margin(egui::Margin::same(12)),
            )
            .show(context, |ui| {
                if let Some(error) = &state.error {
                    theme::callout(ui, theme::Tone::Danger, error);
                }
                let prompts = state.prompts.clone();
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
                    for preset in prompts {
                        if ui
                            .add_enabled_ui(!state.busy, |ui| {
                                theme::icon_chip(ui, IconKind::Bulb, preset.label.clone(), false)
                            })
                            .inner
                            .clicked()
                        {
                            state.prompt = preset.prompt;
                            self.focus_prompt = true;
                        }
                    }
                });
                ui.add_space(4.0);
                let input = ui.add(
                    egui::TextEdit::multiline(&mut state.prompt)
                        .hint_text(
                            egui::RichText::new(tr("quick_ask.prompt_hint"))
                                .color(theme::TEXT_FAINT),
                        )
                        .margin(egui::Margin::symmetric(12, 10))
                        .desired_rows(3)
                        .desired_width(f32::INFINITY),
                );
                if self.focus_prompt {
                    input.request_focus();
                    self.focus_prompt = false;
                }
                let send_key = input.has_focus()
                    && ui
                        .input(|input| input.modifiers.ctrl && input.key_pressed(egui::Key::Enter));
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(theme::faint(tr("quick_ask.input_hint")));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let can_send = !state.busy && !state.prompt.trim().is_empty();
                        if ui
                            .add_enabled(
                                can_send,
                                theme::primary_button(format!(
                                    "{} (Ctrl+Enter)",
                                    tr("quick_ask.send")
                                )),
                            )
                            .clicked()
                            || (can_send && send_key)
                        {
                            let prompt = std::mem::take(&mut state.prompt);
                            state.events.push_back(QuickAskEvent::Submit(prompt));
                        }
                        if state.busy {
                            if ui
                                .add(theme::danger_button(tr("quick_ask.cancel")))
                                .clicked()
                            {
                                state.events.push_back(QuickAskEvent::Cancel);
                            }
                        } else if ui
                            .add(theme::secondary_button(tr("quick_ask.clear")))
                            .clicked()
                        {
                            state.events.push_back(QuickAskEvent::Clear);
                        }
                    });
                });
            });
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::BG)
                    .inner_margin(egui::Margin::symmetric(16, 12)),
            )
            .show(context, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        // Keeps bubbles off the scrollbar and the first message off the top edge.
                        egui::Frame::new()
                            .inner_margin(egui::Margin {
                                left: 0,
                                right: 12,
                                top: 8,
                                bottom: 4,
                            })
                            .show(ui, |ui| {
                                if state.messages.is_empty() {
                                    ui.add_space(24.0);
                                    ui.vertical_centered(|ui| {
                                        ui.label(theme::faint(tr("quick_ask.empty")));
                                    });
                                }
                                for message in &state.messages {
                                    message_bubble(ui, message);
                                    ui.add_space(6.0);
                                }
                            });
                    });
            });
        if !state.events.is_empty() {
            orbment_ui::wake(self.wake_hwnd);
        }
    }
}

fn channel_badge(
    channel_name: &str,
    provider: QuickAskProvider,
    model: &str,
    effort: ThinkingEffort,
) -> String {
    let channel_name = if channel_name.is_empty() {
        quick_ask_provider_label(provider)
    } else {
        channel_name.to_owned()
    };
    format!(
        "{channel_name} · {model} · {}",
        thinking_effort_label(effort),
    )
}

/// User messages sit right in blue bubbles, replies left in cards, tool calls as compact strips.
fn message_bubble(ui: &mut egui::Ui, message: &ChatLine) {
    let width = ui.available_width();
    if let ChatRole::Tool = message.role {
        egui::Frame::new()
            .fill(theme::TOOL_CHIP)
            .stroke(theme::stroke(1.0, egui::Color32::from_rgb(44, 58, 82)))
            .corner_radius(8)
            .inner_margin(egui::Margin::symmetric(10, 5))
            .show(ui, |ui| {
                ui.set_max_width(width * 0.85);
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    theme::icon(ui, IconKind::Wrench, theme::TEXT_MUTED);
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(format!(
                                "{}: {}",
                                tr("quick_ask.tool_called"),
                                message.text
                            ))
                            .size(12.5)
                            .monospace()
                            .color(theme::TEXT_MUTED),
                        )
                        .wrap(),
                    );
                });
            });
        return;
    }
    let user = matches!(message.role, ChatRole::User);
    let max_text = width * if user { 0.72 } else { 0.86 } - 28.0;
    let font = egui::FontId::proportional(14.0);
    let text_width = ui
        .fonts(|fonts| fonts.layout(message.text.clone(), font, theme::TEXT, max_text))
        .size()
        .x
        .ceil()
        + 1.0;
    let side = if user {
        egui::Align::Max
    } else {
        egui::Align::Min
    };
    ui.with_layout(egui::Layout::top_down(side), |ui| {
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.label(theme::faint(tr(if user {
            "quick_ask.user"
        } else {
            "quick_ask.assistant"
        })));
        egui::Frame::new()
            .fill(if user {
                theme::ACCENT_SOFT
            } else {
                theme::CARD
            })
            .stroke(theme::stroke(
                1.0,
                if user {
                    egui::Color32::from_rgb(45, 72, 115)
                } else {
                    theme::CARD_BORDER
                },
            ))
            .corner_radius(12)
            .inner_margin(egui::Margin::symmetric(14, 10))
            .show(ui, |ui| {
                // Left-aligned text inside the bubble regardless of which side it sits on.
                ui.set_max_width(text_width);
                ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.set_width(text_width);
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(&message.text)
                                .size(14.0)
                                .color(theme::TEXT),
                        )
                        .wrap()
                        .selectable(true),
                    );
                });
            });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn header_shows_effective_channel_model_and_thinking() {
        let state = Arc::new(Mutex::new(QuickAskState {
            channel_name: "我的 Gemini 工作台".into(),
            provider: QuickAskProvider::Google,
            model: "gemini-2.5-flash".into(),
            thinking_effort: ThinkingEffort::High,
            ..Default::default()
        }));
        let mut view = QuickAskView::new(state, HWND::default());
        let context = egui::Context::default();
        let output = context.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1000.0, 600.0),
                )),
                ..Default::default()
            },
            |context| view.ui(context),
        );
        let expected = format!(
            "我的 Gemini 工作台 · gemini-2.5-flash · {}",
            thinking_effort_label(ThinkingEffort::High),
        );
        assert!(output.shapes.iter().any(|shape| {
            matches!(
                &shape.shape,
                egui::Shape::Text(text) if text.galley.text() == expected
            )
        }));
    }

    #[test]
    fn context_preview_truncates_by_unicode_characters_and_flattens_newlines() {
        let boundary = "界é\u{1d11e}\n".repeat(7);
        let flattened = "界é\u{1d11e} ".repeat(7);
        for (text, preview) in [
            ("短\n文".into(), "短 文".into()),
            ("é".repeat(27), "é".repeat(27)),
            (boundary.clone(), flattened.clone()),
            (format!("{boundary}尾"), format!("{flattened}…")),
            (
                format!("{boundary}{}", "尾".repeat(1000)),
                format!("{flattened}…"),
            ),
        ] {
            let state = Arc::new(Mutex::new(QuickAskState {
                context: QuickContext {
                    text,
                    ..Default::default()
                },
                ..Default::default()
            }));
            let mut view = QuickAskView::new(state, HWND::default());
            let context = egui::Context::default();
            let output = context.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1000.0, 600.0),
                    )),
                    ..Default::default()
                },
                |context| view.ui(context),
            );
            let expected = format!("{} · {preview}", tr("quick_ask.context"));
            assert!(output.shapes.iter().any(|shape| {
                matches!(
                    &shape.shape,
                    egui::Shape::Text(text) if text.galley.text() == expected
                )
            }));
        }
    }
}
