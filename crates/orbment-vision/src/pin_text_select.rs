use crate::{Point, Rect};
use orbment_core::ocr::{OcrBoundingBox, OcrResult};
use std::ops::Range;

#[derive(Clone, Debug)]
pub struct CachedCharacter {
    pub text: Range<usize>,
    pub bounds: Rect,
    pub line: usize,
    pub word: usize,
    /// Native Windows OCR only supplies word boxes; subdivisions are estimates.
    pub estimated: bool,
}

#[derive(Clone, Debug)]
pub struct CachedWord {
    pub bounds: Rect,
    pub characters: Range<usize>,
}

#[derive(Clone, Debug)]
pub struct CachedLine {
    pub bounds: Rect,
    pub text: Range<usize>,
    pub words: Range<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerAction {
    MovePin,
    SelectText,
    SelectionMenu,
    None,
}

/// One cache per image revision. Coordinates remain image pixels; window zoom
/// and DPI conversion belong at the pointer boundary, never in the OCR cache.
#[derive(Clone, Debug, Default)]
pub struct PinTextSelection {
    text: String,
    pub lines: Vec<CachedLine>,
    pub words: Vec<CachedWord>,
    pub characters: Vec<CachedCharacter>,
    anchor: Option<usize>,
    focus: Option<usize>,
    dragging: bool,
    drag_origin: Option<Point>,
    drag_moved: bool,
    move_pin: bool,
}

impl PinTextSelection {
    pub fn new(result: OcrResult) -> Self {
        let mut cache = Self::default();
        for (line_index, line) in result.lines.iter().enumerate() {
            if line_index > 0 {
                cache.text.push('\n');
            }
            let line_start = cache.text.len();
            cache.text.push_str(&line.text);
            let word_start = cache.words.len();
            let mut search_from = 0;
            for word in &line.words {
                if word.text.is_empty() {
                    continue;
                }
                // Use OCR line text as authority so punctuation and spaces copy
                // exactly as recognized rather than being synthesized per glyph.
                let Some(relative) = line.text[search_from..].find(&word.text) else {
                    continue;
                };
                let text_start = line_start + search_from + relative;
                search_from += relative + word.text.len();
                let first_character = cache.characters.len();
                let count = word.text.chars().count();
                let vertical =
                    word.bounding_box.height > word.bounding_box.width * 1.8 && count > 1;
                let bounds = rect(word.bounding_box);
                let word_index = cache.words.len();
                for (ordinal, (offset, ch)) in word.text.char_indices().enumerate() {
                    let cell = if vertical {
                        Rect {
                            x: bounds.x,
                            y: bounds.y + bounds.height * ordinal as f64 / count as f64,
                            width: bounds.width,
                            height: bounds.height / count as f64,
                        }
                    } else {
                        Rect {
                            x: bounds.x + bounds.width * ordinal as f64 / count as f64,
                            y: bounds.y,
                            width: bounds.width / count as f64,
                            height: bounds.height,
                        }
                    };
                    cache.characters.push(CachedCharacter {
                        text: text_start + offset..text_start + offset + ch.len_utf8(),
                        bounds: cell,
                        line: line_index,
                        word: word_index,
                        estimated: count > 1,
                    });
                }
                cache.words.push(CachedWord {
                    bounds,
                    characters: first_character..cache.characters.len(),
                });
            }
            // Some engines return only line boxes. Preserve selection without
            // inventing word boundaries; character boxes are explicitly estimated.
            if cache.words.len() == word_start && !line.text.is_empty() {
                let count = line.text.chars().count();
                let bounds = rect(line.bounding_box);
                let vertical = bounds.height > bounds.width * 1.8 && count > 1;
                let first = cache.characters.len();
                for (ordinal, (offset, ch)) in line.text.char_indices().enumerate() {
                    cache.characters.push(CachedCharacter {
                        text: line_start + offset..line_start + offset + ch.len_utf8(),
                        bounds: if vertical {
                            Rect {
                                x: bounds.x,
                                y: bounds.y + bounds.height * ordinal as f64 / count as f64,
                                width: bounds.width,
                                height: bounds.height / count as f64,
                            }
                        } else {
                            Rect {
                                x: bounds.x + bounds.width * ordinal as f64 / count as f64,
                                y: bounds.y,
                                width: bounds.width / count as f64,
                                height: bounds.height,
                            }
                        },
                        line: line_index,
                        word: word_start,
                        estimated: true,
                    });
                }
                cache.words.push(CachedWord {
                    bounds,
                    characters: first..cache.characters.len(),
                });
            }
            cache.lines.push(CachedLine {
                bounds: rect(line.bounding_box),
                text: line_start..cache.text.len(),
                words: word_start..cache.words.len(),
            });
        }
        if cache.lines.is_empty() {
            cache.text = result.text;
        }
        cache
    }

    pub fn all_text(&self) -> &str {
        &self.text
    }

    pub fn hit_test(&self, point: Point, alt: bool) -> Option<usize> {
        if alt {
            return None;
        }
        self.characters
            .iter()
            .position(|character| character.bounds.contains(point))
    }

    pub fn text_cursor(&self, point: Point, alt: bool) -> bool {
        self.hit_test(point, alt).is_some()
    }

    pub fn pointer_down(&mut self, point: Point, alt: bool) -> PointerAction {
        self.dragging = false;
        self.move_pin = alt;
        self.drag_origin = None;
        self.drag_moved = false;
        if alt {
            return PointerAction::MovePin;
        }
        if let Some(index) = self.hit_test(point, false) {
            self.anchor = Some(index);
            self.focus = Some(index);
            self.dragging = true;
            self.drag_origin = Some(point);
            PointerAction::SelectText
        } else {
            self.clear();
            self.move_pin = true;
            PointerAction::MovePin
        }
    }

    /// Once Alt starts a pin drag, releasing Alt must not steal that gesture.
    pub fn pointer_move(&mut self, point: Point, alt: bool) -> PointerAction {
        if self.move_pin || alt {
            self.dragging = false;
            self.move_pin = true;
            return PointerAction::MovePin;
        }
        if !self.dragging {
            return PointerAction::None;
        }
        self.focus = self.hit_test(point, false).or_else(|| {
            self.characters
                .iter()
                .enumerate()
                .min_by(|(_, a), (_, b)| {
                    distance_to_rect(point, a.bounds).total_cmp(&distance_to_rect(point, b.bounds))
                })
                .map(|(index, _)| index)
        });
        self.drag_moved |= self.focus != self.anchor
            || self
                .drag_origin
                .is_some_and(|origin| (point.x - origin.x).hypot(point.y - origin.y) >= 2.0);
        PointerAction::SelectText
    }

    pub fn pointer_up(&mut self) -> PointerAction {
        let result = if self.move_pin {
            PointerAction::MovePin
        } else if self.dragging && self.drag_moved && self.selected_range().is_some() {
            PointerAction::SelectionMenu
        } else {
            PointerAction::None
        };
        if self.dragging && !self.drag_moved {
            self.anchor = None;
            self.focus = None;
        }
        self.drag_origin = None;
        self.drag_moved = false;
        self.move_pin = false;
        self.dragging = false;
        result
    }

    pub fn select_word(&mut self, point: Point, alt: bool) -> PointerAction {
        if alt {
            return PointerAction::MovePin;
        }
        if let Some(index) = self.hit_test(point, false) {
            let word = &self.words[self.characters[index].word];
            self.anchor = Some(word.characters.start);
            self.focus = word.characters.end.checked_sub(1);
            PointerAction::SelectionMenu
        } else {
            PointerAction::None
        }
    }

    pub fn selected_range(&self) -> Option<Range<usize>> {
        let (a, b) = (self.anchor?, self.focus?);
        Some(self.characters[a.min(b)].text.start..self.characters[a.max(b)].text.end)
    }

    pub fn selected_text(&self) -> &str {
        self.selected_range().map_or("", |range| &self.text[range])
    }

    pub fn copy_selected(&self, owner: isize) -> anyhow::Result<()> {
        orbment_core::utilities::copy_text(owner, self.selected_text())
    }

    pub fn copy_all(&self, owner: isize) -> anyhow::Result<()> {
        orbment_core::utilities::copy_text(owner, self.all_text())
    }

    pub fn highlight_rects(&self) -> Vec<Rect> {
        let (Some(anchor), Some(focus)) = (self.anchor, self.focus) else {
            return Vec::new();
        };
        let mut result: Vec<Rect> = Vec::new();
        let mut last_word = None;
        for glyph in &self.characters[anchor.min(focus)..=anchor.max(focus)] {
            if last_word == Some(glyph.word) {
                let last = result.last_mut().expect("highlight group exists");
                let right = (last.x + last.width).max(glyph.bounds.x + glyph.bounds.width);
                let bottom = (last.y + last.height).max(glyph.bounds.y + glyph.bounds.height);
                last.x = last.x.min(glyph.bounds.x);
                last.y = last.y.min(glyph.bounds.y);
                last.width = right - last.x;
                last.height = bottom - last.y;
            } else {
                result.push(glyph.bounds);
            }
            last_word = Some(glyph.word);
        }
        result
    }

    pub fn clear(&mut self) {
        self.anchor = None;
        self.focus = None;
        self.dragging = false;
    }
}

fn rect(bounds: OcrBoundingBox) -> Rect {
    Rect {
        x: bounds.x as f64,
        y: bounds.y as f64,
        width: bounds.width as f64,
        height: bounds.height as f64,
    }
}

fn distance_to_rect(point: Point, rect: Rect) -> f64 {
    let dx = (rect.x - point.x)
        .max(0.0)
        .max(point.x - rect.x - rect.width);
    let dy = (rect.y - point.y)
        .max(0.0)
        .max(point.y - rect.y - rect.height);
    dx * dx + dy * dy
}

#[cfg(test)]
mod tests {
    use super::*;
    use orbment_core::ocr::{OcrLine, OcrWord};
    fn cache() -> PinTextSelection {
        let bounds = OcrBoundingBox {
            x: 10.0,
            y: 20.0,
            width: 40.0,
            height: 10.0,
        };
        PinTextSelection::new(OcrResult {
            text: "中文ab".into(),
            lines: vec![OcrLine {
                text: "中文ab".into(),
                bounding_box: bounds,
                words: vec![OcrWord {
                    text: "中文ab".into(),
                    bounding_box: bounds,
                }],
            }],
        })
    }
    #[test]
    fn hit_selection_preserves_unicode_and_reverse_drag() {
        let mut cache = cache();
        assert_eq!(
            cache.pointer_down(Point { x: 45.0, y: 25.0 }, false),
            PointerAction::SelectText
        );
        cache.pointer_move(Point { x: 25.0, y: 25.0 }, false);
        assert_eq!(cache.pointer_up(), PointerAction::SelectionMenu);
        assert_eq!(cache.selected_text(), "文ab");
        assert_eq!(
            cache.highlight_rects(),
            vec![Rect {
                x: 20.0,
                y: 20.0,
                width: 30.0,
                height: 10.0
            }]
        );
        assert!(!cache.text_cursor(Point { x: 5.0, y: 5.0 }, false));
    }
    #[test]
    fn a_click_does_not_open_selection_menu_but_a_drag_within_one_character_does() {
        let mut cache = cache();
        let point = Point { x: 15.0, y: 25.0 };
        cache.pointer_down(point, false);
        cache.pointer_move(point, false);
        assert_eq!(cache.pointer_up(), PointerAction::None);
        assert_eq!(cache.selected_text(), "");
        cache.pointer_down(point, false);
        cache.pointer_move(Point { x: 18.0, y: 25.0 }, false);
        assert_eq!(cache.pointer_up(), PointerAction::SelectionMenu);
        assert_eq!(cache.selected_text(), "中");
    }
    #[test]
    fn alt_bypasses_text_and_latches_pin_drag() {
        let mut cache = cache();
        let point = Point { x: 15.0, y: 25.0 };
        assert!(!cache.text_cursor(point, true));
        assert_eq!(cache.pointer_down(point, true), PointerAction::MovePin);
        assert_eq!(cache.pointer_move(point, false), PointerAction::MovePin);
        assert_eq!(cache.pointer_up(), PointerAction::MovePin);
        assert_eq!(cache.selected_text(), "");
    }
}
