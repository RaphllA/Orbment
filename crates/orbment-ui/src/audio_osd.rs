use crate::{D2dContext, OverlayFloatingWindow, windows::OsdPlacement};
use anyhow::Result;
use orbment_core::{config::AudioConfig, i18n::tr, lifecycle::UiSurfaceState};
use std::{collections::VecDeque, rc::Rc};

pub struct AudioOsd {
    graphics: Rc<D2dContext>,
    banners: VecDeque<OverlayFloatingWindow>,
    microphone: Option<OverlayFloatingWindow>,
}

impl AudioOsd {
    pub fn new(graphics: Rc<D2dContext>) -> Self {
        Self {
            graphics,
            banners: VecDeque::new(),
            microphone: None,
        }
    }

    pub fn show(&mut self, config: &AudioConfig, text: &str) -> Result<()> {
        self.retain_alive();
        let limit = config.max_number_notifications as usize;
        if limit == 0 {
            return Ok(());
        }
        while self.banners.len() >= limit {
            self.banners.pop_front();
        }
        let text = match config.banner_display_info.as_str() {
            "IconOnly" => tr("audio.osd_symbol"),
            "TextOnly" => text.to_owned(),
            _ => format!("{}  {text}", tr("audio.osd_symbol")),
        };
        self.banners.push_back(OverlayFloatingWindow::new(
            self.graphics.clone(),
            &text,
            config.banner_on_screen_time_ms,
        )?);
        for (index, banner) in self.banners.iter().enumerate() {
            banner.configure(placement(config, index))?;
        }
        Ok(())
    }

    pub fn set_microphone_state(
        &mut self,
        config: &AudioConfig,
        muted: bool,
        mode: &str,
    ) -> Result<()> {
        self.microphone = None;
        if mode == "None" {
            return Ok(());
        }
        let text = tr(if muted {
            "audio.microphone_muted_badge"
        } else {
            "audio.microphone_live_badge"
        });
        let badge = OverlayFloatingWindow::new(
            self.graphics.clone(),
            &text,
            config.banner_on_screen_time_ms,
        )?;
        let mut badge_placement = placement(config, 0);
        badge_placement.position = "BottomRight".into();
        if config.banner_position == "BottomRight" {
            badge_placement.stack = config.max_number_notifications as usize;
        }
        badge.configure(badge_placement)?;
        if mode == "Persistent" {
            badge.set_duration(None)?;
        }
        self.microphone = Some(badge);
        Ok(())
    }

    pub fn retain_alive(&mut self) {
        self.banners.retain(OverlayFloatingWindow::is_alive);
        if self
            .microphone
            .as_ref()
            .is_some_and(|badge| !badge.is_alive())
        {
            self.microphone = None;
        }
    }

    pub fn clear(&mut self) {
        self.banners.clear();
        self.microphone = None;
    }
    pub fn ui_state(&self) -> UiSurfaceState {
        if self.banners.iter().any(OverlayFloatingWindow::is_alive)
            || self
                .microphone
                .as_ref()
                .is_some_and(OverlayFloatingWindow::is_alive)
        {
            UiSurfaceState::Visible
        } else {
            UiSurfaceState::Unloaded
        }
    }
}

fn placement(config: &AudioConfig, stack: usize) -> OsdPlacement {
    OsdPlacement {
        position: config.banner_position.clone(),
        custom: config.custom_banner_position,
        primary: config.notify_using_primary_screen,
        stack,
        opacity: config.banner_opacity_pct,
    }
}
