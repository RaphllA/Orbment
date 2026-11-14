use crate::arrow_comment::{
    ArrowComment, ArrowCommentEditor, CommentPlacement, FontStyle, distance, placement,
};
use crate::{CapturedImage, Point, Rect};
use anyhow::{Context, Result, ensure};
use orbment_core::ocr::OcrResult;
use serde::{Deserialize, Serialize};
use tiny_skia::{
    FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, PixmapPaint, Stroke, Transform,
};
use windows::{
    Win32::{
        Foundation::{COLORREF, RECT},
        Graphics::Gdi::*,
    },
    core::PCWSTR,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MosaicMode {
    Pixel,
    Gaussian,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MarkStyle {
    pub argb: u32,
    pub width: f64,
    pub fill: bool,
    pub font: FontStyle,
}
impl Default for MarkStyle {
    fn default() -> Self {
        Self {
            argb: 0xff5e91e6,
            width: 3.0,
            fill: false,
            font: FontStyle::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum MarkKind {
    Rectangle {
        bounds: Rect,
    },
    RoundedRectangle {
        bounds: Rect,
        radius: f64,
    },
    Ellipse {
        bounds: Rect,
    },
    Spotlight {
        bounds: Rect,
        outside_argb: u32,
    },
    Brush {
        points: Vec<Point>,
    },
    Highlighter {
        points: Vec<Point>,
    },
    Arrow {
        points: Vec<Point>,
        comment: Option<ArrowComment>,
    },
    Polyline {
        points: Vec<Point>,
        comment: Option<ArrowComment>,
    },
    Magnifier {
        center: Point,
        radius: f64,
        zoom: f64,
        source: Point,
    },
    Text {
        position: Point,
        text: String,
        angle: f64,
    },
    /// Variables are expanded once at insertion, so restored pins retain their timestamp.
    Watermark {
        bounds: Rect,
        text: String,
        angle: f64,
        spacing: f64,
    },
    Sequence {
        center: Point,
        number: u32,
        radius: f64,
    },
    Mosaic {
        bounds: Rect,
        mode: MosaicMode,
        strength: f64,
    },
    AutoMosaic {
        regions: Vec<Rect>,
        mode: MosaicMode,
        strength: f64,
    },
    Eraser {
        points: Vec<Point>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Mark {
    pub id: u64,
    pub kind: MarkKind,
    pub style: MarkStyle,
}

impl Mark {
    pub fn points(&self) -> Option<&[Point]> {
        match &self.kind {
            MarkKind::Arrow { points, .. }
            | MarkKind::Polyline { points, .. }
            | MarkKind::Brush { points }
            | MarkKind::Highlighter { points }
            | MarkKind::Eraser { points } => Some(points),
            _ => None,
        }
    }
    pub fn points_mut(&mut self) -> Option<&mut Vec<Point>> {
        match &mut self.kind {
            MarkKind::Arrow { points, .. }
            | MarkKind::Polyline { points, .. }
            | MarkKind::Brush { points }
            | MarkKind::Highlighter { points }
            | MarkKind::Eraser { points } => Some(points),
            _ => None,
        }
    }
    pub fn translate(&mut self, dx: f64, dy: f64) {
        let shift = |p: &mut Point| {
            p.x += dx;
            p.y += dy;
        };
        let rect_shift = |r: &mut Rect| {
            r.x += dx;
            r.y += dy;
        };
        match &mut self.kind {
            MarkKind::Arrow { points, .. }
            | MarkKind::Polyline { points, .. }
            | MarkKind::Brush { points }
            | MarkKind::Highlighter { points }
            | MarkKind::Eraser { points } => points.iter_mut().for_each(shift),
            MarkKind::Rectangle { bounds }
            | MarkKind::RoundedRectangle { bounds, .. }
            | MarkKind::Ellipse { bounds }
            | MarkKind::Spotlight { bounds, .. }
            | MarkKind::Mosaic { bounds, .. }
            | MarkKind::Watermark { bounds, .. } => rect_shift(bounds),
            MarkKind::AutoMosaic { regions, .. } => regions.iter_mut().for_each(rect_shift),
            MarkKind::Magnifier { center, .. } | MarkKind::Sequence { center, .. } => shift(center),
            MarkKind::Text { position, .. } => shift(position),
        }
    }
    pub fn bounds(&self) -> Rect {
        match &self.kind {
            MarkKind::Rectangle { bounds }
            | MarkKind::RoundedRectangle { bounds, .. }
            | MarkKind::Ellipse { bounds }
            | MarkKind::Spotlight { bounds, .. }
            | MarkKind::Mosaic { bounds, .. }
            | MarkKind::Watermark { bounds, .. } => *bounds,
            MarkKind::AutoMosaic { regions, .. } => {
                regions.iter().copied().reduce(union).unwrap_or_default()
            }
            MarkKind::Magnifier { center, radius, .. }
            | MarkKind::Sequence { center, radius, .. } => Rect {
                x: center.x - radius,
                y: center.y - radius,
                width: radius * 2.0,
                height: radius * 2.0,
            },
            MarkKind::Text { position, text, .. } => Rect {
                x: position.x,
                y: position.y,
                width: text.chars().count() as f64 * self.style.font.size,
                height: self.style.font.size * 1.5,
            },
            _ => self
                .points()
                .unwrap_or_default()
                .iter()
                .map(|p| Rect {
                    x: p.x,
                    y: p.y,
                    width: 0.0,
                    height: 0.0,
                })
                .reduce(union)
                .unwrap_or_default(),
        }
    }
    /// Clockwise box handles start at the top-left; paths expose their vertices.
    pub fn control_points(&self) -> Vec<Point> {
        match &self.kind {
            MarkKind::Arrow { points, .. } | MarkKind::Polyline { points, .. } => points.clone(),
            MarkKind::Magnifier { center, radius, .. }
            | MarkKind::Sequence { center, radius, .. } => vec![Point {
                x: center.x + radius,
                y: center.y,
            }],
            _ => {
                let bounds = self.bounds();
                let xs = [
                    bounds.x,
                    bounds.x + bounds.width * 0.5,
                    bounds.x + bounds.width,
                ];
                let ys = [
                    bounds.y,
                    bounds.y + bounds.height * 0.5,
                    bounds.y + bounds.height,
                ];
                [
                    (0, 0),
                    (1, 0),
                    (2, 0),
                    (2, 1),
                    (2, 2),
                    (1, 2),
                    (0, 2),
                    (0, 1),
                ]
                .map(|(x, y)| Point { x: xs[x], y: ys[y] })
                .to_vec()
            }
        }
    }
    pub fn handle_at(&self, point: Point, tolerance: f64) -> Option<usize> {
        self.control_points()
            .iter()
            .enumerate()
            .map(|(index, handle)| (index, distance(*handle, point)))
            .filter(|(_, distance)| *distance <= tolerance)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(index, _)| index)
    }
    pub fn resize_by_handle(&mut self, handle: usize, point: Point) {
        let before = self.bounds();
        match &mut self.kind {
            MarkKind::Arrow { points, .. } | MarkKind::Polyline { points, .. } => {
                if let Some(vertex) = points.get_mut(handle) {
                    *vertex = point;
                }
            }
            MarkKind::Magnifier { center, radius, .. }
            | MarkKind::Sequence { center, radius, .. } => {
                if handle == 0 {
                    *radius = distance(*center, point).clamp(8.0, 400.0);
                }
            }
            _ if handle >= 8 => {}
            MarkKind::Rectangle { bounds }
            | MarkKind::RoundedRectangle { bounds, .. }
            | MarkKind::Ellipse { bounds }
            | MarkKind::Mosaic { bounds, .. }
            | MarkKind::Spotlight { bounds, .. }
            | MarkKind::Watermark { bounds, .. } => {
                *bounds = resized_bounds(*bounds, handle, point);
            }
            MarkKind::Text { position, text, .. } => {
                let target = resized_bounds(before, handle, point);
                let scale = match handle {
                    1 | 5 => target.height / before.height.max(1.0),
                    3 | 7 => target.width / before.width.max(1.0),
                    _ => (target.width / before.width.max(1.0))
                        .min(target.height / before.height.max(1.0)),
                };
                self.style.font.size = (self.style.font.size * scale).clamp(2.0, 400.0);
                let width = text.chars().count() as f64 * self.style.font.size;
                let height = self.style.font.size * 1.5;
                position.x = if matches!(handle, 0 | 6 | 7) {
                    before.x + before.width - width
                } else {
                    before.x
                };
                position.y = if matches!(handle, 0..=2) {
                    before.y + before.height - height
                } else {
                    before.y
                };
            }
            MarkKind::Brush { points }
            | MarkKind::Highlighter { points }
            | MarkKind::Eraser { points } => {
                let target = resized_bounds(before, handle, point);
                for vertex in points {
                    *vertex = remap_point(*vertex, before, target);
                }
            }
            MarkKind::AutoMosaic { regions, .. } => {
                let target = resized_bounds(before, handle, point);
                for region in regions {
                    let origin = remap_point(
                        Point {
                            x: region.x,
                            y: region.y,
                        },
                        before,
                        target,
                    );
                    region.x = origin.x;
                    region.y = origin.y;
                    region.width *= target.width / before.width.max(1.0);
                    region.height *= target.height / before.height.max(1.0);
                }
            }
        }
    }
    pub fn hit_test(&self, p: Point, tolerance: f64) -> bool {
        let tolerance = tolerance + self.style.width * 0.5;
        if let Some(points) = self.points() {
            if points.len() == 1 {
                return distance(points[0], p) <= tolerance;
            }
            return points
                .windows(2)
                .any(|pair| segment_distance(p, pair[0], pair[1]) <= tolerance);
        }
        let bounds = self.bounds();
        if !expand(bounds, tolerance).contains(p) {
            return false;
        }
        match self.kind {
            MarkKind::Rectangle { .. } if !self.style.fill => {
                !expand(bounds, -tolerance).contains(p)
            }
            MarkKind::RoundedRectangle { radius, .. } => {
                let radius = radius.clamp(0.0, bounds.width.min(bounds.height) * 0.5);
                let dx =
                    (p.x - bounds.x - bounds.width * 0.5).abs() - (bounds.width * 0.5 - radius);
                let dy =
                    (p.y - bounds.y - bounds.height * 0.5).abs() - (bounds.height * 0.5 - radius);
                let edge = dx.max(0.0).hypot(dy.max(0.0)) + dx.max(dy).min(0.0) - radius;
                if self.style.fill {
                    edge <= tolerance
                } else {
                    edge.abs() <= tolerance
                }
            }
            MarkKind::Ellipse { .. } => {
                let rx = bounds.width * 0.5;
                let ry = bounds.height * 0.5;
                if rx <= 0.0 || ry <= 0.0 {
                    return false;
                }
                let norm = ((p.x - bounds.x - rx) / rx).hypot((p.y - bounds.y - ry) / ry);
                if self.style.fill {
                    norm <= 1.0 + tolerance / rx.min(ry)
                } else {
                    (norm - 1.0).abs() <= tolerance / rx.min(ry)
                }
            }
            MarkKind::AutoMosaic { ref regions, .. } => {
                regions.iter().any(|r| expand(*r, tolerance).contains(p))
            }
            _ => true,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Scene {
    marks: Vec<Mark>,
    #[serde(skip)]
    selected: Option<u64>,
    #[serde(skip)]
    undo: Vec<Vec<Mark>>,
    #[serde(skip)]
    redo: Vec<Vec<Mark>>,
    #[serde(skip)]
    transaction: Option<Vec<Mark>>,
}

impl Scene {
    pub fn marks(&self) -> &[Mark] {
        &self.marks
    }
    pub fn mark(&self, id: u64) -> Option<&Mark> {
        self.marks.iter().find(|mark| mark.id == id)
    }
    pub fn selected(&self) -> Option<u64> {
        self.selected
    }
    pub fn selected_mark(&self) -> Option<&Mark> {
        self.selected.and_then(|id| self.mark(id))
    }
    pub fn clear_selection(&mut self) {
        self.selected = None;
    }
    pub fn hit_test_or_selected(&self, point: Point, tolerance: f64) -> Option<u64> {
        self.marks
            .iter()
            .rev()
            .find(|mark| {
                mark.hit_test(point, tolerance)
                    || (self.selected == Some(mark.id) && mark.bounds().contains(point))
            })
            .map(|mark| mark.id)
    }
    fn checkpoint(&mut self) {
        if self.transaction.is_some() {
            return;
        }
        self.undo.push(self.marks.clone());
        if self.undo.len() > 100 {
            self.undo.remove(0);
        }
        self.redo.clear();
    }
    pub fn add(&mut self, mut kind: MarkKind, style: MarkStyle) -> u64 {
        self.checkpoint();
        let id = self.marks.iter().map(|mark| mark.id).max().unwrap_or(0) + 1;
        if let MarkKind::Watermark { text, .. } = &mut kind {
            *text = crate::save_template::expand_text(
                text,
                &crate::save_template::SaveTemplateContext::now("", id),
            );
        }
        self.marks.push(Mark { id, kind, style });
        self.selected = Some(id);
        id
    }
    pub fn add_sequence(&mut self, center: Point, radius: f64, style: MarkStyle) -> u64 {
        let number = self
            .marks
            .iter()
            .filter_map(|m| match m.kind {
                MarkKind::Sequence { number, .. } => Some(number),
                _ => None,
            })
            .max()
            .unwrap_or(0)
            + 1;
        self.add(
            MarkKind::Sequence {
                center,
                number,
                radius,
            },
            style,
        )
    }
    pub fn auto_mosaic(
        &mut self,
        result: &OcrResult,
        region: Option<Rect>,
        mode: MosaicMode,
        strength: f64,
    ) -> Option<u64> {
        let regions: Vec<_> = result
            .lines
            .iter()
            .map(|line| {
                let b = line.bounding_box;
                expand(
                    Rect {
                        x: b.x as f64,
                        y: b.y as f64,
                        width: b.width as f64,
                        height: b.height as f64,
                    },
                    2.0,
                )
            })
            .filter_map(|r| region.map_or(Some(r), |limit| intersection(r, limit)))
            .collect();
        if regions.is_empty() {
            None
        } else {
            Some(self.add(
                MarkKind::AutoMosaic {
                    regions,
                    mode,
                    strength,
                },
                MarkStyle::default(),
            ))
        }
    }
    pub fn highlight(&mut self, regions: &[Rect], argb: u32) {
        self.begin_edit();
        for r in regions {
            self.add(
                MarkKind::Highlighter {
                    points: vec![
                        Point {
                            x: r.x,
                            y: r.y + r.height * 0.5,
                        },
                        Point {
                            x: r.x + r.width,
                            y: r.y + r.height * 0.5,
                        },
                    ],
                },
                MarkStyle {
                    argb,
                    width: r.height,
                    ..Default::default()
                },
            );
        }
        self.commit_edit();
    }
    pub fn select(&mut self, point: Point, tolerance: f64) -> Option<u64> {
        self.selected = self.hit_test_or_selected(point, tolerance);
        self.selected
    }
    pub fn edit_selected(&mut self, edit: impl FnOnce(&mut Mark)) {
        if let Some(index) = self
            .selected
            .and_then(|id| self.marks.iter().position(|m| m.id == id))
        {
            self.checkpoint();
            edit(&mut self.marks[index]);
        }
    }
    /// Bracket a drag gesture to make all of its pointer moves one undo step.
    pub fn begin_edit(&mut self) {
        if self.transaction.is_none() {
            self.transaction = Some(self.marks.clone());
        }
    }
    pub fn commit_edit(&mut self) {
        if let Some(before) = self.transaction.take()
            && before != self.marks
        {
            self.undo.push(before);
            if self.undo.len() > 100 {
                self.undo.remove(0);
            }
            self.redo.clear();
        }
    }
    pub fn cancel_edit(&mut self) {
        if let Some(before) = self.transaction.take() {
            self.marks = before;
        }
    }
    pub fn delete_selected(&mut self) {
        if let Some(id) = self.selected.take() {
            self.checkpoint();
            self.marks.retain(|m| m.id != id);
        }
    }
    pub fn erase(&mut self, point: Point, radius: f64) {
        if self.marks.iter().any(|m| m.hit_test(point, radius)) {
            self.checkpoint();
            self.marks.retain(|m| !m.hit_test(point, radius));
            self.selected = None;
        }
    }
    pub fn undo(&mut self) -> bool {
        self.cancel_edit();
        if let Some(before) = self.undo.pop() {
            self.redo.push(std::mem::replace(&mut self.marks, before));
            self.selected = None;
            true
        } else {
            false
        }
    }
    pub fn redo(&mut self) -> bool {
        self.cancel_edit();
        if let Some(after) = self.redo.pop() {
            self.undo.push(std::mem::replace(&mut self.marks, after));
            self.selected = None;
            true
        } else {
            false
        }
    }
    pub fn double_click_comment(
        &mut self,
        point: Point,
        tolerance: f64,
        enabled: bool,
    ) -> Option<ArrowCommentEditor> {
        if !enabled {
            return None;
        }
        let mark = self.marks.iter().rev().find(|m| {
            matches!(m.kind, MarkKind::Arrow { .. } | MarkKind::Polyline { .. })
                && m.hit_test(point, tolerance)
        })?;
        let comment = match &mark.kind {
            MarkKind::Arrow { comment, .. } | MarkKind::Polyline { comment, .. } => {
                comment.clone().unwrap_or_default()
            }
            _ => unreachable!(),
        };
        self.selected = Some(mark.id);
        Some(ArrowCommentEditor::new(mark.id, comment))
    }
    pub fn double_click_text(
        &mut self,
        point: Point,
        tolerance: f64,
    ) -> Option<ArrowCommentEditor> {
        let mark = self.marks.iter().rev().find(|mark| {
            matches!(
                mark.kind,
                MarkKind::Text { .. } | MarkKind::Watermark { .. }
            ) && mark.hit_test(point, tolerance)
        })?;
        let text = match &mark.kind {
            MarkKind::Text { text, .. } | MarkKind::Watermark { text, .. } => text.clone(),
            _ => unreachable!(),
        };
        let comment = ArrowComment {
            text,
            orientation: crate::arrow_comment::Orientation::Flat,
            font: mark.style.font.clone(),
        };
        self.selected = Some(mark.id);
        Some(ArrowCommentEditor::new(mark.id, comment))
    }
    pub fn apply_comment(&mut self, id: u64, comment: ArrowComment) {
        let Some(index) = self.marks.iter().position(|m| m.id == id) else {
            return;
        };
        match &self.marks[index].kind {
            MarkKind::Arrow { comment: value, .. } | MarkKind::Polyline { comment: value, .. } => {
                if value.as_ref() == Some(&comment)
                    || (value.is_none() && comment == ArrowComment::default())
                {
                    return;
                }
            }
            MarkKind::Text { text, .. } | MarkKind::Watermark { text, .. } => {
                if *text == comment.text && self.marks[index].style.font == comment.font {
                    return;
                }
            }
            _ => return,
        }
        self.checkpoint();
        let mark = &mut self.marks[index];
        match &mut mark.kind {
            MarkKind::Arrow { comment: value, .. } | MarkKind::Polyline { comment: value, .. } => {
                *value = Some(comment)
            }
            MarkKind::Text { text, .. } | MarkKind::Watermark { text, .. } => {
                *text = comment.text;
                mark.style.font = comment.font;
            }
            _ => {}
        }
    }
    pub fn render(&self, image: &CapturedImage) -> Result<CapturedImage> {
        self.render_preview(image, None)
    }
    pub fn render_preview(
        &self,
        image: &CapturedImage,
        editor: Option<&ArrowCommentEditor>,
    ) -> Result<CapturedImage> {
        self.render_scene(image, editor, false)
    }
    pub fn render_with_selection(
        &self,
        image: &CapturedImage,
        editor: Option<&ArrowCommentEditor>,
    ) -> Result<CapturedImage> {
        self.render_scene(image, editor, true)
    }
    fn render_scene(
        &self,
        image: &CapturedImage,
        editor: Option<&ArrowCommentEditor>,
        show_selection: bool,
    ) -> Result<CapturedImage> {
        image.validate()?;
        if self.marks.is_empty() && (!show_selection || self.selected.is_none()) {
            return Ok(image.clone());
        }
        let mut rgba = image.pixels.clone();
        for pixel in rgba.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
            for c in 0..3 {
                pixel[c] = ((pixel[c] as u32 * pixel[3] as u32 + 127) / 255) as u8;
            }
        }
        let mut canvas = Pixmap::from_vec(
            rgba,
            tiny_skia::IntSize::from_wh(image.width, image.height)
                .context("Invalid scene dimensions")?,
        )
        .context("Cannot allocate scene")?;
        let original = canvas.clone();
        let mut selection = None;
        for mark in &self.marks {
            let preview = editor
                .filter(|editor| editor.mark_id == mark.id)
                .map(|editor| {
                    let mut mark = mark.clone();
                    let preview = editor.preview();
                    match &mut mark.kind {
                        MarkKind::Arrow { comment, .. } | MarkKind::Polyline { comment, .. } => {
                            *comment = Some(preview)
                        }
                        MarkKind::Text { text, .. } | MarkKind::Watermark { text, .. } => {
                            *text = preview.text;
                            mark.style.font = preview.font;
                        }
                        _ => {}
                    }
                    mark
                });
            let mark = preview.as_ref().unwrap_or(mark);
            draw_mark(&mut canvas, &original, mark)?;
            if show_selection && self.selected == Some(mark.id) {
                selection = Some(mark.clone());
            }
        }
        if let Some(mark) = selection {
            draw_selection(&mut canvas, &mark);
        }
        let mut pixels = canvas.take();
        for pixel in pixels.as_chunks_mut::<4>().0 {
            if pixel[3] > 0 {
                for c in 0..3 {
                    pixel[c] = ((pixel[c] as u32 * 255 + pixel[3] as u32 / 2) / pixel[3] as u32)
                        .min(255) as u8;
                }
            }
            pixel.swap(0, 2);
        }
        Ok(CapturedImage {
            width: image.width,
            height: image.height,
            pixels,
        })
    }
}

fn resized_bounds(bounds: Rect, handle: usize, point: Point) -> Rect {
    let mut left = bounds.x;
    let mut top = bounds.y;
    let mut right = left + bounds.width;
    let mut bottom = top + bounds.height;
    if matches!(handle, 0 | 6 | 7) {
        left = point.x.min(right - 2.0);
    }
    if matches!(handle, 2..=4) {
        right = point.x.max(left + 2.0);
    }
    if matches!(handle, 0..=2) {
        top = point.y.min(bottom - 2.0);
    }
    if matches!(handle, 4..=6) {
        bottom = point.y.max(top + 2.0);
    }
    Rect {
        x: left,
        y: top,
        width: (right - left).max(2.0),
        height: (bottom - top).max(2.0),
    }
}

fn remap_point(point: Point, before: Rect, after: Rect) -> Point {
    Point {
        x: after.x + (point.x - before.x) * after.width / before.width.max(1.0),
        y: after.y + (point.y - before.y) * after.height / before.height.max(1.0),
    }
}

fn rounded_path(bounds: Rect, radius: f64) -> Option<tiny_skia::Path> {
    let rect = skia_rect(bounds)?;
    let radius = (radius as f32).clamp(0.0, rect.width().min(rect.height()) * 0.5);
    let control = radius * 0.552_284_8;
    let (left, top, right, bottom) = (rect.left(), rect.top(), rect.right(), rect.bottom());
    let mut path = PathBuilder::new();
    path.move_to(left + radius, top);
    path.line_to(right - radius, top);
    path.cubic_to(
        right - radius + control,
        top,
        right,
        top + radius - control,
        right,
        top + radius,
    );
    path.line_to(right, bottom - radius);
    path.cubic_to(
        right,
        bottom - radius + control,
        right - radius + control,
        bottom,
        right - radius,
        bottom,
    );
    path.line_to(left + radius, bottom);
    path.cubic_to(
        left + radius - control,
        bottom,
        left,
        bottom - radius + control,
        left,
        bottom - radius,
    );
    path.line_to(left, top + radius);
    path.cubic_to(
        left,
        top + radius - control,
        left + radius - control,
        top,
        left + radius,
        top,
    );
    path.close();
    path.finish()
}

fn draw_selection(canvas: &mut Pixmap, mark: &Mark) {
    let blue = color_paint(0xff1677ff);
    let outline = Stroke {
        width: 1.0,
        dash: tiny_skia::StrokeDash::new(vec![4.0, 3.0], 0.0),
        ..Default::default()
    };
    if !matches!(
        mark.kind,
        MarkKind::Arrow { .. } | MarkKind::Polyline { .. }
    ) && let Some(rect) = skia_rect(mark.bounds())
    {
        canvas.stroke_path(
            &PathBuilder::from_rect(rect),
            &blue,
            &outline,
            Transform::identity(),
            None,
        );
    }
    let stroke = Stroke {
        width: 1.5,
        ..Default::default()
    };
    for point in mark.control_points() {
        if let Some(rect) =
            tiny_skia::Rect::from_xywh(point.x as f32 - 4.0, point.y as f32 - 4.0, 8.0, 8.0)
        {
            let path = PathBuilder::from_rect(rect);
            canvas.fill_path(
                &path,
                &color_paint(0xffffffff),
                FillRule::Winding,
                Transform::identity(),
                None,
            );
            canvas.stroke_path(&path, &blue, &stroke, Transform::identity(), None);
        }
    }
}

fn draw_mark(canvas: &mut Pixmap, original: &Pixmap, mark: &Mark) -> Result<()> {
    let style = &mark.style;
    let paint = color_paint(style.argb);
    let stroke = Stroke {
        width: style.width.max(0.1) as f32,
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        ..Default::default()
    };
    match &mark.kind {
        MarkKind::Rectangle { bounds }
        | MarkKind::RoundedRectangle { bounds, .. }
        | MarkKind::Ellipse { bounds } => {
            if let Some(rect) = skia_rect(*bounds) {
                let path = match mark.kind {
                    MarkKind::Ellipse { .. } => PathBuilder::from_oval(rect),
                    MarkKind::RoundedRectangle { radius, .. } => rounded_path(*bounds, radius),
                    _ => Some(PathBuilder::from_rect(rect)),
                };
                if let Some(path) = path {
                    if style.fill {
                        canvas.fill_path(
                            &path,
                            &paint,
                            FillRule::Winding,
                            Transform::identity(),
                            None,
                        );
                    } else {
                        canvas.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
                    }
                }
            }
        }
        MarkKind::Spotlight {
            bounds,
            outside_argb,
        } => {
            let whole = Rect {
                x: 0.0,
                y: 0.0,
                width: canvas.width() as f64,
                height: canvas.height() as f64,
            };
            let mut builder = PathBuilder::new();
            if let Some(rect) = skia_rect(whole) {
                builder.push_rect(rect);
            }
            if let Some(rect) = skia_rect(*bounds) {
                builder.push_rect(rect);
            }
            if let Some(path) = builder.finish() {
                canvas.fill_path(
                    &path,
                    &color_paint(*outside_argb),
                    FillRule::EvenOdd,
                    Transform::identity(),
                    None,
                );
            }
        }
        MarkKind::Brush { points }
        | MarkKind::Highlighter { points }
        | MarkKind::Eraser { points } => {
            let mut color = style.argb;
            if matches!(mark.kind, MarkKind::Highlighter { .. }) {
                color = (color & 0xffffff) | ((color >> 24).min(90) << 24);
            }
            if matches!(mark.kind, MarkKind::Eraser { .. }) {
                let mut mask = Pixmap::new(canvas.width(), canvas.height())
                    .context("Cannot allocate eraser mask")?;
                draw_path(&mut mask, points, &color_paint(0xffffffff), &stroke);
                for ((dst, src), alpha) in canvas
                    .data_mut()
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .zip(original.data().as_chunks::<4>().0)
                    .zip(mask.data().as_chunks::<4>().0)
                {
                    let a = alpha[3] as u32;
                    for c in 0..4 {
                        dst[c] =
                            ((dst[c] as u32 * (255 - a) + src[c] as u32 * a + 127) / 255) as u8;
                    }
                }
            } else {
                draw_path(canvas, points, &color_paint(color), &stroke);
            }
        }
        MarkKind::Arrow { points, comment } | MarkKind::Polyline { points, comment } => {
            let label = comment
                .as_ref()
                .filter(|c| !c.text.is_empty())
                .map(|c| text_bitmap(&c.text, &c.font, style.argb))
                .transpose()?;
            let pos = comment
                .as_ref()
                .and_then(|c| placement(points, c.orientation));
            if let (Some(label), Some(pos)) = (&label, pos) {
                // Cut the shaft only where it intersects the rotated label box.
                for pair in points.windows(2) {
                    draw_gapped_segment(
                        canvas,
                        pair[0],
                        pair[1],
                        pos,
                        (label.width() as f64 + 8.0, label.height() as f64 + 4.0),
                        &paint,
                        &stroke,
                    );
                }
            } else {
                draw_path(canvas, points, &paint, &stroke);
            }
            if matches!(mark.kind, MarkKind::Arrow { .. })
                && let Some(pair) = points
                    .windows(2)
                    .rev()
                    .find(|p| distance(p[0], p[1]) > 0.01)
            {
                let angle = (pair[1].y - pair[0].y).atan2(pair[1].x - pair[0].x);
                let length = (style.width * 4.0).max(10.0);
                let end = pair[1];
                let left = Point {
                    x: end.x - length * (angle - 0.5).cos(),
                    y: end.y - length * (angle - 0.5).sin(),
                };
                let right = Point {
                    x: end.x - length * (angle + 0.5).cos(),
                    y: end.y - length * (angle + 0.5).sin(),
                };
                draw_path(canvas, &[left, end, right], &paint, &stroke);
            }
            if let (Some(label), Some(pos)) = (label, pos) {
                draw_label(canvas, &label, pos.center, pos.angle);
            }
        }
        MarkKind::Magnifier {
            center,
            radius,
            zoom,
            source,
        } => {
            let before = canvas.clone();
            let bounds = Rect {
                x: center.x - radius,
                y: center.y - radius,
                width: radius * 2.0,
                height: radius * 2.0,
            };
            let (x0, y0, x1, y1) = pixel_bounds(bounds, canvas);
            let width = canvas.width() as usize;
            for y in y0..y1 {
                for x in x0..x1 {
                    if (x as f64 - center.x).hypot(y as f64 - center.y) > *radius {
                        continue;
                    }
                    let sx = (source.x + (x as f64 - center.x) / zoom.max(1.0))
                        .clamp(0.0, before.width() as f64 - 1.0)
                        as usize;
                    let sy = (source.y + (y as f64 - center.y) / zoom.max(1.0))
                        .clamp(0.0, before.height() as f64 - 1.0)
                        as usize;
                    let src = (sy * width + sx) * 4;
                    let dst = (y * width + x) * 4;
                    canvas.data_mut()[dst..dst + 4].copy_from_slice(&before.data()[src..src + 4]);
                }
            }
            if let Some(rect) = skia_rect(bounds)
                && let Some(path) = PathBuilder::from_oval(rect)
            {
                canvas.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
            }
        }
        MarkKind::Text {
            position,
            text,
            angle,
        } => {
            let label = text_bitmap(text, &style.font, style.argb)?;
            draw_label(
                canvas,
                &label,
                Point {
                    x: position.x + label.width() as f64 * 0.5,
                    y: position.y + label.height() as f64 * 0.5,
                },
                *angle,
            );
        }
        MarkKind::Watermark {
            bounds,
            text,
            angle,
            spacing,
        } => {
            let label = text_bitmap(text, &style.font, style.argb)?;
            let step_x = label.width() as f64 + spacing.max(1.0);
            let step_y = label.height() as f64 + spacing.max(1.0);
            let mut layer = Pixmap::new(canvas.width(), canvas.height())
                .context("Cannot allocate watermark")?;
            let (x0, y0, x1, y1) = pixel_bounds(*bounds, canvas);
            let mut y = y0 as f64;
            while y < y1 as f64 {
                let mut x = x0 as f64;
                while x < x1 as f64 {
                    draw_label(
                        &mut layer,
                        &label,
                        Point {
                            x: x + label.width() as f64 * 0.5,
                            y: y + label.height() as f64 * 0.5,
                        },
                        *angle,
                    );
                    x += step_x;
                }
                y += step_y;
            }
            let width = canvas.width() as usize;
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = (y * width + x) * 4;
                    composite(&mut canvas.data_mut()[i..i + 4], &layer.data()[i..i + 4]);
                }
            }
        }
        MarkKind::Sequence {
            center,
            number,
            radius,
        } => {
            if let Some(path) =
                PathBuilder::from_circle(center.x as f32, center.y as f32, *radius as f32)
            {
                canvas.fill_path(
                    &path,
                    &paint,
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
            }
            let mut font = style.font.clone();
            font.size = radius * 1.2;
            font.bold = true;
            draw_label(
                canvas,
                &text_bitmap(&number.to_string(), &font, 0xffffffff)?,
                *center,
                0.0,
            );
        }
        MarkKind::Mosaic {
            bounds,
            mode,
            strength,
        } => mosaic(canvas, *bounds, *mode, *strength),
        MarkKind::AutoMosaic {
            regions,
            mode,
            strength,
        } => {
            for bounds in regions {
                mosaic(canvas, *bounds, *mode, *strength);
            }
        }
    }
    Ok(())
}

fn color_paint(argb: u32) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color_rgba8(
        (argb >> 16) as u8,
        (argb >> 8) as u8,
        argb as u8,
        (argb >> 24) as u8,
    );
    paint
}
fn skia_rect(r: Rect) -> Option<tiny_skia::Rect> {
    tiny_skia::Rect::from_xywh(r.x as f32, r.y as f32, r.width as f32, r.height as f32)
}
fn draw_path(canvas: &mut Pixmap, points: &[Point], paint: &Paint<'_>, stroke: &Stroke) {
    if let Some(first) = points.first() {
        if points.len() == 1 {
            if let Some(path) =
                PathBuilder::from_circle(first.x as f32, first.y as f32, stroke.width * 0.5)
            {
                canvas.fill_path(&path, paint, FillRule::Winding, Transform::identity(), None);
            }
            return;
        }
        let mut path = PathBuilder::new();
        path.move_to(first.x as f32, first.y as f32);
        for point in &points[1..] {
            path.line_to(point.x as f32, point.y as f32);
        }
        if let Some(path) = path.finish() {
            canvas.stroke_path(&path, paint, stroke, Transform::identity(), None);
        }
    }
}
fn draw_gapped_segment(
    canvas: &mut Pixmap,
    a: Point,
    b: Point,
    placement: CommentPlacement,
    size: (f64, f64),
    paint: &Paint<'_>,
    stroke: &Stroke,
) {
    let CommentPlacement { center, angle, .. } = placement;
    let (width, height) = size;
    let local = |p: Point| Point {
        x: (p.x - center.x) * angle.cos() + (p.y - center.y) * angle.sin(),
        y: -(p.x - center.x) * angle.sin() + (p.y - center.y) * angle.cos(),
    };
    let (p, q) = (local(a), local(b));
    let mut enter: f64 = 0.0;
    let mut exit: f64 = 1.0;
    for (start, delta, half) in [
        (p.x, q.x - p.x, width * 0.5),
        (p.y, q.y - p.y, height * 0.5),
    ] {
        if delta.abs() < 1e-9 {
            if start.abs() > half {
                draw_path(canvas, &[a, b], paint, stroke);
                return;
            }
        } else {
            let u = (-half - start) / delta;
            let v = (half - start) / delta;
            enter = enter.max(u.min(v));
            exit = exit.min(u.max(v));
        }
    }
    if enter >= exit {
        draw_path(canvas, &[a, b], paint, stroke);
        return;
    }
    let at = |t: f64| Point {
        x: a.x + (b.x - a.x) * t,
        y: a.y + (b.y - a.y) * t,
    };
    if enter > 0.0 {
        draw_path(canvas, &[a, at(enter)], paint, stroke);
    }
    if exit < 1.0 {
        draw_path(canvas, &[at(exit), b], paint, stroke);
    }
}
fn draw_label(canvas: &mut Pixmap, label: &Pixmap, center: Point, angle: f64) {
    let (sin, cos) = angle.sin_cos();
    let x = label.width() as f64 * 0.5;
    let y = label.height() as f64 * 0.5;
    let transform = Transform::from_row(
        cos as f32,
        sin as f32,
        -sin as f32,
        cos as f32,
        (center.x - x * cos + y * sin) as f32,
        (center.y - x * sin - y * cos) as f32,
    );
    canvas.draw_pixmap(
        0,
        0,
        label.as_ref(),
        &PixmapPaint {
            quality: tiny_skia::FilterQuality::Bilinear,
            ..Default::default()
        },
        transform,
        None,
    );
}

pub(crate) fn render_ui_text(text: &str, size_px: f64, bold: bool, argb: u32) -> Result<Pixmap> {
    let bitmap = text_bitmap(
        text,
        &FontStyle {
            family: "Microsoft YaHei UI".into(),
            size: size_px,
            bold,
            ..Default::default()
        },
        argb,
    )?;
    let (mut left, mut top, mut right, mut bottom) = (bitmap.width(), bitmap.height(), 0, 0);
    for (index, pixel) in bitmap.pixels().iter().enumerate() {
        if pixel.alpha() != 0 {
            let x = index as u32 % bitmap.width();
            let y = index as u32 / bitmap.width();
            left = left.min(x);
            top = top.min(y);
            right = right.max(x + 1);
            bottom = bottom.max(y + 1);
        }
    }
    if right == 0 {
        return Pixmap::new(1, 1).context("Cannot allocate empty text");
    }
    bitmap
        .clone_rect(
            tiny_skia::IntRect::from_xywh(left as i32, top as i32, right - left, bottom - top)
                .context("Invalid text bounds")?,
        )
        .context("Cannot crop UI text")
}

/// GDI supplies system font fallback and shaping; the grayscale mask is composed
/// into premultiplied pixels, so text never relies on GDI's undefined DIB alpha.
fn text_bitmap(text: &str, style: &FontStyle, argb: u32) -> Result<Pixmap> {
    if text.is_empty() {
        return Pixmap::new(1, 1).context("Cannot allocate empty text bitmap");
    }
    struct Resources {
        dc: HDC,
        font: HFONT,
        bitmap: HBITMAP,
        old_font: HGDIOBJ,
        old_bitmap: HGDIOBJ,
    }
    impl Drop for Resources {
        fn drop(&mut self) {
            unsafe {
                if !self.old_font.is_invalid() {
                    SelectObject(self.dc, self.old_font);
                }
                if !self.old_bitmap.is_invalid() {
                    SelectObject(self.dc, self.old_bitmap);
                }
                if !self.font.is_invalid() {
                    let _ = DeleteObject(self.font.into());
                }
                if !self.bitmap.is_invalid() {
                    let _ = DeleteObject(self.bitmap.into());
                }
                let _ = DeleteDC(self.dc);
            }
        }
    }
    unsafe {
        let dc = CreateCompatibleDC(None);
        ensure!(!dc.is_invalid(), "CreateCompatibleDC failed");
        let mut resources = Resources {
            dc,
            font: HFONT::default(),
            bitmap: HBITMAP::default(),
            old_font: HGDIOBJ::default(),
            old_bitmap: HGDIOBJ::default(),
        };
        let face: Vec<u16> = style.family.encode_utf16().chain(Some(0)).collect();
        resources.font = CreateFontW(
            -((style.size.clamp(1.0, 512.0) * 2.0).round() as i32),
            0,
            0,
            0,
            if style.bold {
                FW_SEMIBOLD.0 as i32
            } else {
                FW_NORMAL.0 as i32
            },
            u32::from(style.italic),
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            0,
            PCWSTR(face.as_ptr()),
        );
        ensure!(!resources.font.is_invalid(), "CreateFontW failed");
        resources.old_font = SelectObject(dc, resources.font.into());
        let mut utf16: Vec<u16> = text.encode_utf16().collect();
        let mut bounds = RECT::default();
        DrawTextW(dc, &mut utf16, &mut bounds, DT_CALCRECT | DT_NOPREFIX);
        let padding = (style.outline.clamp(0.0, 16.0).ceil() as i32 + 3) * 2;
        // Even high-resolution bounds make every output pixel an exact 2×2 box average.
        let width = ((bounds.right + padding * 2 + 1).clamp(2, 32768) & !1) as u32;
        let height = ((bounds.bottom + padding * 2 + 1).clamp(2, 16384) & !1) as u32;
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        resources.bitmap = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
        resources.old_bitmap = SelectObject(dc, resources.bitmap.into());
        let mask =
            std::slice::from_raw_parts_mut(bits.cast::<u8>(), width as usize * height as usize * 4);
        mask.fill(0);
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, COLORREF(0x00ffffff));
        bounds = RECT {
            left: padding,
            top: padding,
            right: width as i32 - padding,
            bottom: height as i32 - padding,
        };
        DrawTextW(dc, &mut utf16, &mut bounds, DT_NOPREFIX);
        GdiFlush().ok()?;
        let mut output = Pixmap::new(width, height).context("Cannot allocate text bitmap")?;
        if let Some(background) = style.background_argb {
            output.fill(tiny_skia::Color::from_rgba8(
                (background >> 16) as u8,
                (background >> 8) as u8,
                background as u8,
                (background >> 24) as u8,
            ));
        }
        let radius = (style.outline.clamp(0.0, 16.0) * 2.0).ceil() as i32;
        for y in 0..height as i32 {
            for x in 0..width as i32 {
                let index = (y as usize * width as usize + x as usize) * 4;
                if radius > 0 {
                    let mut coverage = 0;
                    for dy in -radius..=radius {
                        for dx in -radius..=radius {
                            if dx * dx + dy * dy > radius * radius {
                                continue;
                            }
                            let (sx, sy) = (x + dx, y + dy);
                            if sx >= 0 && sy >= 0 && sx < width as i32 && sy < height as i32 {
                                coverage = coverage
                                    .max(mask[(sy as usize * width as usize + sx as usize) * 4]);
                            }
                        }
                    }
                    blend_color(
                        &mut output.data_mut()[index..index + 4],
                        style.outline_argb,
                        coverage,
                    );
                }
                blend_color(&mut output.data_mut()[index..index + 4], argb, mask[index]);
            }
        }
        let mut downsampled =
            Pixmap::new(width / 2, height / 2).context("Cannot allocate supersampled text")?;
        for y in 0..height as usize / 2 {
            for x in 0..width as usize / 2 {
                let source = (y * 2 * width as usize + x * 2) * 4;
                let target = (y * (width as usize / 2) + x) * 4;
                for channel in 0..4 {
                    let row = width as usize * 4;
                    let sum = u16::from(output.data()[source + channel])
                        + u16::from(output.data()[source + 4 + channel])
                        + u16::from(output.data()[source + row + channel])
                        + u16::from(output.data()[source + row + 4 + channel]);
                    downsampled.data_mut()[target + channel] = ((sum + 2) / 4) as u8;
                }
            }
        }
        Ok(downsampled)
    }
}
fn blend_color(dst: &mut [u8], argb: u32, coverage: u8) {
    let alpha = (argb >> 24) * coverage as u32 / 255;
    let source = [
        (((argb >> 16) & 255) * alpha / 255) as u8,
        (((argb >> 8) & 255) * alpha / 255) as u8,
        ((argb & 255) * alpha / 255) as u8,
        alpha as u8,
    ];
    composite(dst, &source);
}
fn composite(dst: &mut [u8], source: &[u8]) {
    let inverse = 255 - source[3] as u32;
    for c in 0..4 {
        dst[c] = (source[c] as u32 + (dst[c] as u32 * inverse + 127) / 255).min(255) as u8;
    }
}
fn pixel_bounds(rect: Rect, canvas: &Pixmap) -> (usize, usize, usize, usize) {
    (
        rect.x.floor().clamp(0.0, canvas.width() as f64) as usize,
        rect.y.floor().clamp(0.0, canvas.height() as f64) as usize,
        (rect.x + rect.width)
            .ceil()
            .clamp(0.0, canvas.width() as f64) as usize,
        (rect.y + rect.height)
            .ceil()
            .clamp(0.0, canvas.height() as f64) as usize,
    )
}
fn mosaic(canvas: &mut Pixmap, bounds: Rect, mode: MosaicMode, strength: f64) {
    let (x0, y0, x1, y1) = pixel_bounds(bounds, canvas);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let stride = canvas.width() as usize;
    match mode {
        MosaicMode::Pixel => {
            let size = strength.round().clamp(2.0, 256.0) as usize;
            for y in (y0..y1).step_by(size) {
                for x in (x0..x1).step_by(size) {
                    let end_x = (x + size).min(x1);
                    let end_y = (y + size).min(y1);
                    let mut sum = [0u64; 4];
                    for sy in y..end_y {
                        for sx in x..end_x {
                            for (c, sum) in sum.iter_mut().enumerate() {
                                *sum += canvas.data()[(sy * stride + sx) * 4 + c] as u64;
                            }
                        }
                    }
                    let count = ((end_x - x) * (end_y - y)) as u64;
                    for sy in y..end_y {
                        for sx in x..end_x {
                            for (c, sum) in sum.iter().enumerate() {
                                canvas.data_mut()[(sy * stride + sx) * 4 + c] = (sum / count) as u8;
                            }
                        }
                    }
                }
            }
        }
        MosaicMode::Gaussian => {
            let sigma = strength.clamp(0.5, 64.0);
            let radius = (sigma * 3.0).ceil() as isize;
            let mut kernel: Vec<f64> = (-radius..=radius)
                .map(|i| (-0.5 * (i as f64 / sigma).powi(2)).exp())
                .collect();
            let total: f64 = kernel.iter().sum();
            for k in &mut kernel {
                *k /= total;
            }
            let source = canvas.data().to_vec();
            let width = x1 - x0;
            let height = y1 - y0;
            let mut temp = vec![[0.0f64; 4]; width * height];
            for y in y0..y1 {
                for x in x0..x1 {
                    for (offset, weight) in (-radius..=radius).zip(&kernel) {
                        let sx = (x as isize + offset).clamp(x0 as isize, x1 as isize - 1) as usize;
                        for c in 0..4 {
                            temp[(y - y0) * width + x - x0][c] +=
                                source[(y * stride + sx) * 4 + c] as f64 * weight;
                        }
                    }
                }
            }
            for y in y0..y1 {
                for x in x0..x1 {
                    let mut value = [0.0f64; 4];
                    for (offset, weight) in (-radius..=radius).zip(&kernel) {
                        let sy = (y as isize + offset).clamp(y0 as isize, y1 as isize - 1) as usize;
                        for c in 0..4 {
                            value[c] += temp[(sy - y0) * width + x - x0][c] * weight;
                        }
                    }
                    for (c, value) in value.iter().enumerate() {
                        canvas.data_mut()[(y * stride + x) * 4 + c] =
                            value.round().clamp(0.0, 255.0) as u8;
                    }
                }
            }
        }
    }
}
fn segment_distance(p: Point, a: Point, b: Point) -> f64 {
    let length = (b.x - a.x).powi(2) + (b.y - a.y).powi(2);
    let t = if length > 0.0 {
        (((p.x - a.x) * (b.x - a.x) + (p.y - a.y) * (b.y - a.y)) / length).clamp(0.0, 1.0)
    } else {
        0.0
    };
    distance(
        p,
        Point {
            x: a.x + t * (b.x - a.x),
            y: a.y + t * (b.y - a.y),
        },
    )
}
fn expand(rect: Rect, amount: f64) -> Rect {
    Rect {
        x: rect.x - amount,
        y: rect.y - amount,
        width: (rect.width + amount * 2.0).max(0.0),
        height: (rect.height + amount * 2.0).max(0.0),
    }
}
fn union(a: Rect, b: Rect) -> Rect {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    Rect {
        x,
        y,
        width: (a.x + a.width).max(b.x + b.width) - x,
        height: (a.y + a.height).max(b.y + b.height) - y,
    }
}
fn intersection(a: Rect, b: Rect) -> Option<Rect> {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let right = (a.x + a.width).min(b.x + b.width);
    let bottom = (a.y + a.height).min(b.y + b.height);
    (right > x && bottom > y).then_some(Rect {
        x,
        y,
        width: right - x,
        height: bottom - y,
    })
}

#[cfg(test)]
mod text_tests {
    use super::*;

    #[test]
    fn ui_text_uses_measured_ink_bounds_and_antialiased_premultiplied_pixels() -> Result<()> {
        let narrow = render_ui_text("iii", 24., true, 0xffffffff)?;
        let wide = render_ui_text("WWW", 24., true, 0xffffffff)?;
        assert!(wide.width() > narrow.width() * 2);
        let text = render_ui_text("录屏 Capture", 20., true, 0xff1677ff)?;
        assert!(
            text.pixels()
                .iter()
                .any(|p| p.alpha() > 0 && p.alpha() < 255)
        );
        assert!(
            text.pixels()
                .iter()
                .all(|p| p.red() <= p.alpha() && p.green() <= p.alpha() && p.blue() <= p.alpha())
        );
        let pixels = text.pixels();
        let width = text.width() as usize;
        assert!(pixels[..width].iter().any(|p| p.alpha() != 0));
        assert!(
            pixels[pixels.len() - width..]
                .iter()
                .any(|p| p.alpha() != 0)
        );
        assert!((0..text.height() as usize).any(|y| pixels[y * width].alpha() != 0));
        assert!((0..text.height() as usize).any(|y| pixels[y * width + width - 1].alpha() != 0));
        Ok(())
    }
}

#[cfg(test)]
mod editing_tests {
    use super::*;

    #[test]
    fn empty_scene_preserves_straight_alpha_pixels_in_exports_and_previews() -> Result<()> {
        let image = CapturedImage {
            width: 4,
            height: 1,
            pixels: vec![
                17, 83, 149, 0, 17, 83, 149, 1, 23, 95, 167, 7, 31, 127, 213, 255,
            ],
        };
        let scene = Scene::default();
        for rendered in [
            scene.render(&image)?,
            scene.render_preview(&image, None)?,
            scene.render_with_selection(&image, None)?,
        ] {
            assert_eq!(
                (rendered.width, rendered.height),
                (image.width, image.height)
            );
            assert_eq!(rendered.pixels, image.pixels);
        }
        Ok(())
    }

    #[test]
    fn empty_scene_still_rejects_invalid_images() {
        let scene = Scene::default();
        for image in [
            CapturedImage {
                width: 0,
                height: 1,
                pixels: Vec::new(),
            },
            CapturedImage {
                width: 1,
                height: 1,
                pixels: vec![255; 3],
            },
            CapturedImage {
                width: u32::MAX,
                height: u32::MAX,
                pixels: Vec::new(),
            },
        ] {
            assert!(scene.render_preview(&image, None).is_err());
            assert!(scene.render_with_selection(&image, None).is_err());
        }
    }

    fn bounds() -> Rect {
        Rect {
            x: 20.0,
            y: 30.0,
            width: 80.0,
            height: 60.0,
        }
    }

    fn mark(kind: MarkKind) -> Mark {
        Mark {
            id: 1,
            kind,
            style: MarkStyle::default(),
        }
    }

    #[test]
    fn small_marks_choose_the_nearest_overlapping_handle() {
        for size in [2.0, 10.0] {
            let mark = mark(MarkKind::Rectangle {
                bounds: Rect {
                    x: 20.0,
                    y: 30.0,
                    width: size,
                    height: size,
                },
            });
            for (index, point) in mark.control_points().into_iter().enumerate() {
                assert_eq!(mark.handle_at(point, 8.0), Some(index));
            }
        }
    }

    #[test]
    fn text_selection_tracks_live_editor_geometry_without_changing_export() -> Result<()> {
        let image = CapturedImage {
            width: 240,
            height: 120,
            pixels: vec![255; 240 * 120 * 4],
        };
        let mut scene = Scene::default();
        let id = scene.add(
            MarkKind::Text {
                position: Point { x: 20.0, y: 30.0 },
                text: String::new(),
                angle: 0.0,
            },
            MarkStyle::default(),
        );
        let exported = scene.render(&image)?;
        let comment = ArrowComment {
            text: "Preview".into(),
            font: FontStyle {
                size: 24.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let editor = ArrowCommentEditor::new(id, comment.clone());
        let preview = scene.render_with_selection(&image, Some(&editor))?;
        assert_eq!(scene.render(&image)?.pixels, exported.pixels);
        scene.apply_comment(id, comment);
        assert_eq!(
            scene.render_with_selection(&image, None)?.pixels,
            preview.pixels
        );
        Ok(())
    }

    #[test]
    fn area_marks_have_clockwise_handles_and_resize_all_edges() {
        let kinds = [
            MarkKind::Rectangle { bounds: bounds() },
            MarkKind::RoundedRectangle {
                bounds: bounds(),
                radius: 12.0,
            },
            MarkKind::Ellipse { bounds: bounds() },
            MarkKind::Spotlight {
                bounds: bounds(),
                outside_argb: 0x80000000,
            },
            MarkKind::Mosaic {
                bounds: bounds(),
                mode: MosaicMode::Pixel,
                strength: 8.0,
            },
            MarkKind::Watermark {
                bounds: bounds(),
                text: "test".into(),
                angle: 0.0,
                spacing: 10.0,
            },
        ];
        let points = [
            (20.0, 30.0),
            (60.0, 30.0),
            (100.0, 30.0),
            (100.0, 60.0),
            (100.0, 90.0),
            (60.0, 90.0),
            (20.0, 90.0),
            (20.0, 60.0),
        ]
        .map(|(x, y)| Point { x, y });
        for kind in kinds {
            let original = mark(kind);
            assert_eq!(original.control_points(), points);
            for (handle, point) in points.iter().enumerate() {
                assert_eq!(original.handle_at(*point, 8.0), Some(handle));
                let mut resized = original.clone();
                resized.resize_by_handle(
                    handle,
                    Point {
                        x: point.x + 5.0,
                        y: point.y + 6.0,
                    },
                );
                let result = resized.bounds();
                let left = if matches!(handle, 0 | 6 | 7) {
                    25.0
                } else {
                    20.0
                };
                let top = if matches!(handle, 0..=2) { 36.0 } else { 30.0 };
                let right = if matches!(handle, 2..=4) {
                    105.0
                } else {
                    100.0
                };
                let bottom = if matches!(handle, 4..=6) { 96.0 } else { 90.0 };
                assert_eq!(
                    result,
                    Rect {
                        x: left,
                        y: top,
                        width: right - left,
                        height: bottom - top
                    }
                );
            }
            let mut clamped = original.clone();
            clamped.resize_by_handle(0, Point { x: 500.0, y: 500.0 });
            assert_eq!(
                clamped.bounds(),
                Rect {
                    x: 98.0,
                    y: 88.0,
                    width: 2.0,
                    height: 2.0
                }
            );
            clamped.resize_by_handle(99, Point::default());
            assert_eq!(clamped.bounds().width, 2.0);
        }
    }

    #[test]
    fn line_vertices_and_radius_handles_resize_without_moving_other_anchors() {
        let points = vec![Point { x: 10.0, y: 20.0 }, Point { x: 60.0, y: 70.0 }];
        for kind in [
            MarkKind::Arrow {
                points: points.clone(),
                comment: None,
            },
            MarkKind::Polyline {
                points: points.clone(),
                comment: None,
            },
        ] {
            let mut line = mark(kind);
            assert_eq!(line.control_points(), points);
            line.resize_by_handle(1, Point { x: 80.0, y: 90.0 });
            assert_eq!(
                line.points().unwrap(),
                &[points[0], Point { x: 80.0, y: 90.0 }]
            );
            line.resize_by_handle(8, Point::default());
            assert_eq!(line.points().unwrap()[0], points[0]);
        }
        let center = Point { x: 40.0, y: 50.0 };
        for kind in [
            MarkKind::Sequence {
                center,
                number: 1,
                radius: 20.0,
            },
            MarkKind::Magnifier {
                center,
                radius: 20.0,
                zoom: 2.0,
                source: center,
            },
        ] {
            let mut circle = mark(kind);
            assert_eq!(circle.control_points(), vec![Point { x: 60.0, y: 50.0 }]);
            circle.resize_by_handle(0, center);
            assert_eq!(circle.bounds().width, 16.0);
            circle.resize_by_handle(0, Point { x: 40.0, y: 550.0 });
            assert_eq!(circle.bounds().width, 800.0);
            assert_eq!(circle.bounds().x + 400.0, center.x);
        }
    }

    #[test]
    fn text_and_brush_resize_and_translate_as_whole_objects() {
        let mut text = mark(MarkKind::Text {
            position: Point::default(),
            text: "Hello".into(),
            angle: 0.0,
        });
        let size = text.style.font.size;
        let before = text.bounds();
        text.resize_by_handle(
            4,
            Point {
                x: before.width * 2.0,
                y: before.height * 2.0,
            },
        );
        assert_eq!(text.style.font.size, size * 2.0);
        text.translate(12.0, -4.0);
        assert_eq!(text.bounds().x, 12.0);
        assert_eq!(text.bounds().y, -4.0);
        let mut brush = mark(MarkKind::Brush {
            points: vec![Point { x: 10.0, y: 10.0 }, Point { x: 20.0, y: 30.0 }],
        });
        brush.resize_by_handle(4, Point { x: 30.0, y: 50.0 });
        assert_eq!(brush.points().unwrap()[1], Point { x: 30.0, y: 50.0 });
    }

    #[test]
    fn selected_interior_drags_and_delete_are_undoable_transactions() {
        let mut scene = Scene::default();
        let id = scene.add(
            MarkKind::RoundedRectangle {
                bounds: bounds(),
                radius: 12.0,
            },
            MarkStyle::default(),
        );
        let inside = Point { x: 60.0, y: 60.0 };
        assert!(!scene.mark(id).unwrap().hit_test(inside, 8.0));
        assert_eq!(scene.select(inside, 8.0), Some(id));
        let before = scene.mark(id).unwrap().clone();
        scene.begin_edit();
        scene.edit_selected(|mark| mark.translate(3.0, 4.0));
        scene.edit_selected(|mark| mark.translate(7.0, 6.0));
        scene.commit_edit();
        assert_eq!(scene.mark(id).unwrap().bounds().x, before.bounds().x + 10.0);
        assert!(scene.undo());
        assert_eq!(scene.mark(id), Some(&before));
        assert!(scene.redo());
        assert_eq!(scene.select(Point { x: 70.0, y: 40.0 }, 8.0), Some(id));
        scene.delete_selected();
        assert!(scene.marks().is_empty());
        assert_eq!(scene.selected(), None);
        assert!(scene.undo());
        assert_eq!(scene.marks().len(), 1);
        scene.clear_selection();
        assert_eq!(scene.select(Point { x: 70.0, y: 70.0 }, 8.0), None);
    }

    #[test]
    fn rounded_rectangle_is_one_object_and_selection_never_leaks_into_exports() -> Result<()> {
        let image = CapturedImage {
            width: 140,
            height: 120,
            pixels: vec![255; 140 * 120 * 4],
        };
        let mut scene = Scene::default();
        let style = MarkStyle {
            argb: 0xffff0000,
            fill: true,
            ..Default::default()
        };
        scene.add(
            MarkKind::RoundedRectangle {
                bounds: bounds(),
                radius: 20.0,
            },
            style,
        );
        let exported = scene.render(&image)?;
        let restored: Scene = serde_json::from_str(&serde_json::to_string(&scene)?)?;
        assert_eq!(restored.marks(), scene.marks());
        assert_eq!(restored.selected(), None);
        assert_eq!(restored.render(&image)?.pixels, exported.pixels);
        let preview = scene.render_with_selection(&image, None)?;
        assert_ne!(preview.pixels, exported.pixels);
        assert_eq!(scene.render_preview(&image, None)?.pixels, exported.pixels);
        let pixel = |x: usize, y: usize| &exported.pixels[(y * 140 + x) * 4..(y * 140 + x + 1) * 4];
        assert_eq!(pixel(21, 31), &[255, 255, 255, 255]);
        assert_eq!(pixel(60, 60), &[0, 0, 255, 255]);
        scene.clear_selection();
        assert_eq!(scene.render(&image)?.pixels, exported.pixels);
        assert_eq!(
            scene.render_with_selection(&image, None)?.pixels,
            exported.pixels
        );
        assert!(scene.undo());
        assert!(scene.marks().is_empty());
        Ok(())
    }
}
