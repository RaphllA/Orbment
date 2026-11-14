//! `--render-ui-previews <DIR>`: renders the review screenshots through the real GPU path,
//! adding the application-owned QuickAsk popup with a representative conversation.
use anyhow::Result;
use orbment_core::config::QuickAskConfig;
use orbment_quickask::{
    ContextImage, QuickAskState, QuickAskView, QuickContext,
    ui_popup::{ChatLine, ChatRole},
};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
use windows::Win32::Foundation::HWND;

pub fn render(out_dir: &Path) -> Result<()> {
    let line = |role, text: &str| ChatLine {
        role,
        text: text.into(),
    };
    let config = QuickAskConfig::default();
    let state = QuickAskState {
        context: QuickContext {
            text: "fn apply_fluent_dark(context: &egui::Context) {\n    context.set_theme(egui::Theme::Dark);\n}".into(),
            source_title: "theme.rs - Orbment - Visual Studio Code".into(),
            image: Some(ContextImage {
                mime_type: "image/png".into(),
                bytes: vec![0; 46_080],
            }),
        },
        // Sized to fit the chat viewport, so the first message is not cut by stick-to-bottom.
        messages: vec![
            line(ChatRole::User, "解释一下这段主题代码做了什么？"),
            line(
                ChatRole::Assistant,
                "它把 egui 切换到深色主题，并统一了背景、控件与选中项的配色。",
            ),
            line(ChatRole::Tool, "orbment.memory_status {}"),
            line(
                ChatRole::Assistant,
                "当前物理内存已使用 68%，暂不需要清理。",
            ),
        ],
        prompt: "帮我把这段代码改成支持浅色主题".into(),
        channel_name: config.active_channel_config().name.clone(),
        provider: config.provider,
        model: config.model,
        thinking_effort: config.thinking_effort,
        prompts: config.prompts,
        ..Default::default()
    };
    let view = QuickAskView::new(Arc::new(Mutex::new(state)), HWND::default());
    orbment_ui::render_ui_previews(out_dir, vec![("09-quickask-popup".into(), Box::new(view))])?;
    Ok(())
}
