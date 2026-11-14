use std::rc::Rc;

use anyhow::Result;
use orbment_core::config::{MonitorConfig, TaskbarConfig};
use orbment_ui::{D2dContext, TaskbarChildWindow, TextRun};

use crate::service::MonitorItem;

pub struct TaskbarMonitor {
    window: TaskbarChildWindow,
    config: TaskbarConfig,
}

impl TaskbarMonitor {
    pub fn new(context: Rc<D2dContext>, config: &MonitorConfig) -> Result<Self> {
        let taskbar = &config.taskbar;
        let mut window = TaskbarChildWindow::new(
            context,
            "",
            &taskbar.font_name,
            taskbar.size,
            taskbar.text_colors.first().copied().unwrap_or(0x00ff_ffff),
            taskbar.back_color,
        )?;
        window.set_options(
            taskbar.style,
            taskbar.transparent_color,
            taskbar.on_left,
            taskbar.snap,
        )?;
        window.set_grid_options(taskbar)?;
        Ok(Self {
            window,
            config: taskbar.clone(),
        })
    }

    pub fn set_items(&mut self, items: &[MonitorItem]) -> Result<()> {
        let mut items_runs = Vec::with_capacity(items.len());
        for item in items {
            // Taskbar metric slots contain 26 label/value colors indexed by the display bit.
            let palette_index = if self.config.specify_each_item_color {
                item.display_bit.trailing_zeros() as usize * 2
            } else {
                0
            };
            let colors = &self.config.text_colors;
            let label_color = colors
                .get(palette_index)
                .or_else(|| colors.first())
                .copied()
                .unwrap_or(0x00ff_ffff);
            let value_color = colors
                .get(palette_index + 1)
                .or_else(|| colors.get(1))
                .copied()
                .unwrap_or(label_color);
            items_runs.push([
                TextRun {
                    text: item.label.clone(),
                    color: label_color,
                    progress: None,
                    progress_color: self.config.status_bar_color,
                },
                TextRun {
                    text: item.value.clone(),
                    color: value_color,
                    progress: if self.config.show_status_bar {
                        item.usage_fraction
                    } else {
                        None
                    },
                    progress_color: self.config.status_bar_color,
                },
            ]);
        }
        self.window.set_grid_items(items_runs)
    }

    pub fn reattach(&mut self) -> Result<()> {
        self.window.reattach()
    }
}
