use crate::Point;
use orbment_ui::ime::{ImeInput, Imm32};
use serde::{Deserialize, Serialize};
use windows::Win32::{
    Foundation::{HWND, LPARAM, RECT, WPARAM},
    UI::{
        Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_SHIFT},
        WindowsAndMessaging::*,
    },
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Orientation {
    #[default]
    Auto,
    Follow,
    Flat,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FontStyle {
    pub family: String,
    pub size: f64,
    pub bold: bool,
    pub italic: bool,
    pub outline: f64,
    pub outline_argb: u32,
    pub background_argb: Option<u32>,
}

impl Default for FontStyle {
    fn default() -> Self {
        Self {
            family: "Microsoft YaHei UI".into(),
            size: 18.0,
            bold: false,
            italic: false,
            outline: 0.0,
            outline_argb: 0xffffffff,
            background_argb: None,
        }
    }
}

impl From<&orbment_core::config::VisionTextConfig> for FontStyle {
    fn from(config: &orbment_core::config::VisionTextConfig) -> Self {
        Self {
            family: config.font.clone(),
            size: config.size as f64,
            bold: config.bold,
            italic: config.italic,
            outline: if config.outline {
                config.outline_width as f64
            } else {
                0.0
            },
            outline_argb: u32::from_str_radix(&config.outline_color_argb, 16)
                .expect("validated annotation outline color"),
            background_argb: config.background.then(|| {
                u32::from_str_radix(&config.background_color_argb, 16)
                    .expect("validated annotation background color")
            }),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ArrowComment {
    pub text: String,
    pub orientation: Orientation,
    pub font: FontStyle,
}

impl From<&orbment_core::config::VisionStrokeConfig> for ArrowComment {
    fn from(config: &orbment_core::config::VisionStrokeConfig) -> Self {
        Self {
            text: String::new(),
            orientation: match config.text_direction.as_str() {
                "Follow" => Orientation::Follow,
                "Flat" => Orientation::Flat,
                "Auto" => Orientation::Auto,
                _ => unreachable!("validated annotation text direction"),
            },
            font: FontStyle::from(&config.text),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CommentPlacement {
    pub center: Point,
    /// Radians in image coordinates (positive angles turn clockwise).
    pub angle: f64,
    pub distance: f64,
}

/// Arc-length midpoint, rather than the bounding-box or endpoint midpoint, keeps
/// a folded polyline's comment on its actual shaft. Zero-length edges are ignored.
pub fn placement(points: &[Point], orientation: Orientation) -> Option<CommentPlacement> {
    let first = *points.first()?;
    let length: f64 = points.windows(2).map(|p| distance(p[0], p[1])).sum();
    let mut remaining = length * 0.5;
    for pair in points.windows(2) {
        let segment = distance(pair[0], pair[1]);
        if segment <= f64::EPSILON {
            continue;
        }
        if remaining <= segment {
            let t = remaining / segment;
            let mut angle = (pair[1].y - pair[0].y).atan2(pair[1].x - pair[0].x);
            match orientation {
                Orientation::Flat => angle = 0.0,
                Orientation::Auto => {
                    if angle > std::f64::consts::FRAC_PI_2 {
                        angle -= std::f64::consts::PI;
                    }
                    if angle < -std::f64::consts::FRAC_PI_2 {
                        angle += std::f64::consts::PI;
                    }
                }
                Orientation::Follow => {}
            }
            return Some(CommentPlacement {
                center: Point {
                    x: pair[0].x + t * (pair[1].x - pair[0].x),
                    y: pair[0].y + t * (pair[1].y - pair[0].y),
                },
                angle,
                distance: length * 0.5,
            });
        }
        remaining -= segment;
    }
    Some(CommentPlacement {
        center: first,
        angle: 0.0,
        distance: 0.0,
    })
}

pub(crate) fn distance(a: Point, b: Point) -> f64 {
    (a.x - b.x).hypot(a.y - b.y)
}

#[derive(Clone, Debug, PartialEq)]
pub enum EditResult {
    Unhandled,
    Changed,
    Commit(ArrowComment),
    Cancel,
}

/// Owned by the native capture/pin HWND while an arrow is being edited. Call
/// `reposition` on endpoint movement, pan, zoom or DPI changes, then render
/// `preview()` in the scene until Commit replaces the persisted comment.
pub struct ArrowCommentEditor {
    pub mark_id: u64,
    comment: ArrowComment,
    preedit: String,
    cursor: usize,
    anchor: usize,
    composing: bool,
    ime: Imm32,
}

impl ArrowCommentEditor {
    pub fn new(mark_id: u64, comment: ArrowComment) -> Self {
        let cursor = comment.text.len();
        Self {
            mark_id,
            comment,
            preedit: String::new(),
            cursor,
            anchor: cursor,
            composing: false,
            ime: Imm32::default(),
        }
    }

    pub fn comment_mut(&mut self) -> &mut ArrowComment {
        &mut self.comment
    }

    pub fn preview(&self) -> ArrowComment {
        let mut result = self.comment.clone();
        if !self.preedit.is_empty() {
            result.text.replace_range(
                self.cursor.min(self.anchor)..self.cursor.max(self.anchor),
                &self.preedit,
            );
        }
        result
    }

    pub fn reposition(&self, hwnd: HWND, points: &[Point], origin: Point, scale: f64) {
        if let Some(place) = placement(points, self.comment.orientation) {
            let x = ((place.center.x - origin.x) * scale).round() as i32;
            let y = ((place.center.y - origin.y) * scale).round() as i32;
            let height = (self.comment.font.size * scale).ceil().max(1.0) as i32;
            self.ime.set_caret(
                hwnd,
                RECT {
                    left: x,
                    top: y - height / 2,
                    right: x + 2,
                    bottom: y + height / 2,
                },
            );
        }
    }

    fn replace_selection(&mut self, text: &str) {
        let start = self.cursor.min(self.anchor);
        let end = self.cursor.max(self.anchor);
        self.comment.text.replace_range(start..end, text);
        self.cursor = start + text.len();
        self.anchor = self.cursor;
    }

    pub fn handle_message(
        &mut self,
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> EditResult {
        if message == WM_KEYDOWN {
            // Composition keys belong to IMM32, not the annotation editor.
            if self.composing {
                return EditResult::Unhandled;
            }
            let control = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
            let shift = unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
            match wparam.0 {
                0x0d => {
                    self.ime.finish(hwnd);
                    return EditResult::Commit(self.comment.clone());
                }
                0x1b => return EditResult::Cancel,
                0x41 if control => {
                    self.anchor = 0;
                    self.cursor = self.comment.text.len();
                }
                0x08 | 0x2e => {
                    if self.cursor == self.anchor {
                        if wparam.0 == 0x08 {
                            self.anchor = self.comment.text[..self.cursor]
                                .char_indices()
                                .next_back()
                                .map_or(0, |(i, _)| i);
                        } else {
                            self.anchor = self.cursor
                                + self.comment.text[self.cursor..]
                                    .chars()
                                    .next()
                                    .map_or(0, char::len_utf8);
                        }
                    }
                    self.replace_selection("");
                }
                0x25 | 0x27 | 0x24 | 0x23 => {
                    self.cursor = match wparam.0 {
                        0x25 => self.comment.text[..self.cursor]
                            .char_indices()
                            .next_back()
                            .map_or(0, |(i, _)| i),
                        0x27 => {
                            self.cursor
                                + self.comment.text[self.cursor..]
                                    .chars()
                                    .next()
                                    .map_or(0, char::len_utf8)
                        }
                        0x24 => 0,
                        _ => self.comment.text.len(),
                    };
                    if !shift {
                        self.anchor = self.cursor;
                    }
                }
                _ => return EditResult::Unhandled,
            }
            return EditResult::Changed;
        }
        if let Some(events) = self.ime.handle(hwnd, message, wparam, lparam) {
            for event in events {
                match event {
                    ImeInput::Enabled => self.composing = true,
                    ImeInput::Disabled => self.composing = false,
                    ImeInput::Preedit(text) => self.preedit = text,
                    ImeInput::Commit(text) | ImeInput::Character(text) => {
                        self.preedit.clear();
                        self.replace_selection(&text);
                    }
                }
            }
            EditResult::Changed
        } else if message == WM_KILLFOCUS {
            let comment = self.preview();
            self.preedit.clear();
            self.ime.finish(hwnd);
            EditResult::Commit(comment)
        } else {
            EditResult::Unhandled
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn folded_midpoint_follows_path_and_endpoint_edits() {
        let mut points = [
            Point { x: 0.0, y: 0.0 },
            Point { x: 10.0, y: 0.0 },
            Point { x: 10.0, y: 30.0 },
        ];
        let p = placement(&points, Orientation::Follow).unwrap();
        assert_eq!(p.center, Point { x: 10.0, y: 10.0 });
        assert!((p.angle - std::f64::consts::FRAC_PI_2).abs() < 1e-10);
        points[2].y = 50.0;
        assert_eq!(
            placement(&points, Orientation::Flat).unwrap().center,
            Point { x: 10.0, y: 20.0 }
        );
        assert_eq!(placement(&points, Orientation::Flat).unwrap().angle, 0.0);
    }
    #[test]
    fn auto_keeps_leftward_text_upright_and_degenerate_paths_finite() {
        let points = [Point { x: 10.0, y: 0.0 }, Point { x: 0.0, y: 0.0 }];
        assert_eq!(placement(&points, Orientation::Auto).unwrap().angle, 0.0);
        assert_eq!(
            placement(&points, Orientation::Follow).unwrap().angle,
            std::f64::consts::PI
        );
        assert_eq!(
            placement(&[points[0], points[0]], Orientation::Auto)
                .unwrap()
                .angle,
            0.0
        );
        assert!(placement(&[], Orientation::Auto).is_none());
    }
}
