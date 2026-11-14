//! Warm neutral light palette, type ramp and shared form widgets for every egui surface.
use egui::{
    Align, Color32, Context, CornerRadius, FontFamily, FontId, Frame, Id, Layout, Margin, Painter,
    Pos2, Rect, Response, RichText, Sense, Shadow, Shape, Stroke, StrokeKind, TextStyle, Ui, Vec2,
    Widget, WidgetText, pos2, vec2,
};

pub const BG: Color32 = Color32::from_rgb(255, 255, 255);
pub const NAV_BG: Color32 = Color32::from_rgb(247, 246, 243);
pub const NAV_DIVIDER: Color32 = Color32::from_rgb(233, 231, 226);
pub const NAV_HOVER: Color32 = Color32::from_rgb(237, 236, 232);
pub const CARD: Color32 = Color32::from_rgb(250, 249, 247);
pub const CARD_BORDER: Color32 = Color32::from_rgb(229, 227, 221);
pub const CARD_HOVER: Color32 = Color32::from_rgb(243, 242, 238);
pub const CARD_HOVER_BORDER: Color32 = Color32::from_rgb(212, 209, 202);
pub const CARD_SELECTED: Color32 = Color32::from_rgb(238, 236, 230);
pub const ROW_DIVIDER: Color32 = Color32::from_rgb(238, 237, 233);
pub const CONTROL: Color32 = Color32::from_rgb(255, 255, 255);
pub const CONTROL_BORDER: Color32 = Color32::from_rgb(214, 211, 204);
pub const CONTROL_HOVER: Color32 = Color32::from_rgb(245, 244, 240);
pub const CONTROL_HOVER_BORDER: Color32 = Color32::from_rgb(190, 187, 179);
pub const ACCENT: Color32 = Color32::from_rgb(20, 20, 19);
pub const ACCENT_HOVER: Color32 = Color32::from_rgb(45, 45, 42);
pub const ACCENT_SOFT: Color32 = Color32::from_rgb(230, 229, 224);
pub const ACCENT_LIGHT: Color32 = Color32::from_rgb(20, 20, 19);
pub const TOOL_CHIP: Color32 = Color32::from_rgb(240, 239, 234);
pub const TEXT: Color32 = Color32::from_rgb(20, 20, 19);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(115, 114, 108);
pub const TEXT_FAINT: Color32 = Color32::from_rgb(148, 146, 139);
pub const WARNING: Color32 = Color32::from_rgb(180, 83, 9);
pub const WARNING_BG: Color32 = Color32::from_rgb(254, 243, 199);
pub const DANGER: Color32 = Color32::from_rgb(220, 38, 38);
pub const DANGER_BG: Color32 = Color32::from_rgb(254, 226, 226);
pub const SUCCESS: Color32 = Color32::from_rgb(5, 150, 105);
pub const SUCCESS_BG: Color32 = Color32::from_rgb(209, 250, 229);
pub const PURPLE: Color32 = Color32::from_rgb(109, 40, 217);
pub const PURPLE_BG: Color32 = Color32::from_rgb(237, 233, 254);
pub const ORANGE: Color32 = Color32::from_rgb(217, 119, 6);
pub const ORANGE_BG: Color32 = Color32::from_rgb(255, 237, 213);
pub const GREEN: Color32 = Color32::from_rgb(21, 128, 61);
pub const GREEN_BG: Color32 = Color32::from_rgb(220, 252, 231);

pub const ROW_HEIGHT: f32 = 44.0;
pub const COMBO_WIDTH: f32 = 180.0;
pub const NUMBER_WIDTH: f32 = 110.0;
pub const TEXT_WIDTH: f32 = 220.0;

/// Installed by `SharedFonts`; surfaces without it (unit tests) fall back to the regular face.
pub const BOLD_FAMILY: &str = "orbment-bold";
const CARD_ROWS: &str = "orbment.theme.card_rows";

pub const fn stroke(width: f32, color: Color32) -> Stroke {
    Stroke { width, color }
}

/// Outline of every colour swatch, so near-background colours stay visible.
pub const SWATCH_BORDER: Color32 = CONTROL_BORDER;
pub const CODE_BG: Color32 = BG;

/// Hand-drawn line icons; Unicode pictographs render as blurry blobs in Microsoft YaHei.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IconKind {
    Settings,
    Sliders,
    Clipboard,
    Viewfinder,
    Search,
    Speaker,
    Microphone,
    LineChart,
    Wrench,
    Chip,
    Disk,
    Chat,
    Terminal,
    BarChart,
    Pulse,
    Info,
    Play,
    Refresh,
    Recycle,
    Plug,
    Lock,
    Unlock,
    Folder,
    Pin,
    Star,
    Monitor,
    Palette,
    Bolt,
    Bulb,
    Image,
    Note,
    Treemap,
    Ranking,
    PieChart,
    Check,
    Warning,
    Shield,
    Plus,
    Back,
    ChevronDown,
    ChevronRight,
    Copy,
    Trash,
    Puzzle,
}

/// Paints `icon` centred in `rect` on a 14×14 design grid with 1.5px strokes.
pub fn draw_icon(painter: &Painter, rect: Rect, icon: IconKind, color: Color32) {
    let side = rect.width().min(rect.height());
    let scale = side / 14.0;
    let origin = rect.center() - Vec2::splat(side * 0.5);
    let p = |x: f32, y: f32| -> Pos2 { origin + vec2(x, y) * scale };
    let line_stroke = stroke(1.5, color);
    let line = |a: Pos2, b: Pos2| {
        painter.line_segment([a, b], line_stroke);
    };
    let poly = |points: &[(f32, f32)], closed: bool| {
        let points: Vec<Pos2> = points.iter().map(|&(x, y)| p(x, y)).collect();
        painter.add(if closed {
            Shape::closed_line(points, line_stroke)
        } else {
            Shape::line(points, line_stroke)
        });
    };
    let rect_at = |x0: f32, y0: f32, x1: f32, y1: f32, radius: f32| {
        painter.rect_stroke(
            Rect::from_min_max(p(x0, y0), p(x1, y1)),
            CornerRadius::same((radius * scale).round() as u8),
            line_stroke,
            StrokeKind::Middle,
        );
    };
    let circle = |x: f32, y: f32, radius: f32| {
        painter.circle_stroke(p(x, y), radius * scale, line_stroke);
    };
    let dot = |x: f32, y: f32, radius: f32| {
        painter.circle_filled(p(x, y), radius * scale, color);
    };
    // Angles in degrees, clockwise from +x (screen coordinates).
    let arc = |x: f32, y: f32, radius: f32, from: f32, to: f32| {
        let steps = (((to - from).abs() / 12.0).ceil() as usize).max(2);
        let points: Vec<Pos2> = (0..=steps)
            .map(|step| {
                let angle = (from + (to - from) * step as f32 / steps as f32).to_radians();
                p(x + radius * angle.cos(), y + radius * angle.sin())
            })
            .collect();
        painter.add(Shape::line(points, line_stroke));
    };
    match icon {
        IconKind::Settings => {
            circle(7.0, 7.0, 2.2);
            circle(7.0, 7.0, 4.6);
            for step in 0..8 {
                let angle = (step as f32 * 45.0).to_radians();
                let (sin, cos) = angle.sin_cos();
                line(
                    p(7.0 + 4.6 * cos, 7.0 + 4.6 * sin),
                    p(7.0 + 6.4 * cos, 7.0 + 6.4 * sin),
                );
            }
        }
        IconKind::Sliders => {
            for (y, knob) in [(3.5, 9.0), (7.0, 4.5), (10.5, 8.0)] {
                line(p(1.5, y), p(knob - 1.6, y));
                line(p(knob + 1.6, y), p(12.5, y));
                circle(knob, y, 1.6);
            }
        }
        IconKind::Clipboard => {
            rect_at(2.5, 2.5, 11.5, 13.0, 1.5);
            rect_at(5.0, 1.0, 9.0, 3.8, 1.0);
            line(p(5.0, 7.0), p(9.0, 7.0));
            line(p(5.0, 9.8), p(8.0, 9.8));
        }
        IconKind::Viewfinder => {
            for (x, y, dx, dy) in [
                (1.5, 1.5, 1.0, 1.0),
                (12.5, 1.5, -1.0, 1.0),
                (1.5, 12.5, 1.0, -1.0),
                (12.5, 12.5, -1.0, -1.0),
            ] {
                poly(&[(x, y + 3.5 * dy), (x, y), (x + 3.5 * dx, y)], false);
            }
            circle(7.0, 7.0, 2.3);
        }
        IconKind::Search => {
            circle(6.0, 6.0, 4.0);
            line(p(9.0, 9.0), p(12.8, 12.8));
        }
        IconKind::Speaker => {
            poly(
                &[
                    (1.5, 5.0),
                    (4.5, 5.0),
                    (8.0, 2.0),
                    (8.0, 12.0),
                    (4.5, 9.0),
                    (1.5, 9.0),
                ],
                true,
            );
            arc(8.0, 7.0, 2.6, -45.0, 45.0);
            arc(8.0, 7.0, 4.8, -50.0, 50.0);
        }
        IconKind::Microphone => {
            rect_at(5.0, 1.0, 9.0, 8.5, 2.0);
            arc(7.0, 6.5, 4.2, 0.0, 180.0);
            line(p(7.0, 10.7), p(7.0, 13.0));
            line(p(4.5, 13.0), p(9.5, 13.0));
        }
        IconKind::LineChart => {
            poly(&[(1.5, 1.5), (1.5, 12.5), (12.5, 12.5)], false);
            poly(&[(3.5, 9.5), (6.0, 6.0), (8.5, 8.0), (12.0, 3.5)], false);
        }
        IconKind::Wrench => {
            arc(9.6, 4.4, 3.2, 100.0, 350.0);
            line(p(7.4, 6.6), p(2.0, 12.0));
            line(p(8.4, 7.6), p(3.0, 13.0));
        }
        IconKind::Chip => {
            rect_at(3.5, 3.5, 10.5, 10.5, 1.0);
            rect_at(5.8, 5.8, 8.2, 8.2, 0.0);
            for offset in [5.2, 8.8] {
                line(p(offset, 1.0), p(offset, 3.5));
                line(p(offset, 10.5), p(offset, 13.0));
                line(p(1.0, offset), p(3.5, offset));
                line(p(10.5, offset), p(13.0, offset));
            }
        }
        IconKind::Disk => {
            circle(7.0, 7.0, 5.8);
            circle(7.0, 7.0, 1.6);
            arc(7.0, 7.0, 3.8, 200.0, 250.0);
        }
        IconKind::Chat => {
            rect_at(1.5, 2.0, 12.5, 9.8, 3.0);
            poly(&[(4.0, 9.8), (3.5, 12.8), (7.0, 9.8)], false);
            for x in [4.6, 7.0, 9.4] {
                dot(x, 5.9, 0.8);
            }
        }
        IconKind::Terminal => {
            rect_at(1.5, 2.0, 12.5, 12.0, 1.5);
            poly(&[(4.0, 5.0), (6.2, 7.0), (4.0, 9.0)], false);
            line(p(7.5, 9.2), p(10.2, 9.2));
        }
        IconKind::BarChart => {
            rect_at(2.0, 7.5, 4.6, 12.0, 0.5);
            rect_at(5.7, 4.0, 8.3, 12.0, 0.5);
            rect_at(9.4, 1.5, 12.0, 12.0, 0.5);
            line(p(1.0, 13.0), p(13.0, 13.0));
        }
        IconKind::Pulse => {
            poly(
                &[
                    (1.0, 7.5),
                    (4.0, 7.5),
                    (5.6, 3.0),
                    (8.2, 11.5),
                    (9.8, 7.5),
                    (13.0, 7.5),
                ],
                false,
            );
        }
        IconKind::Info => {
            circle(7.0, 7.0, 5.8);
            dot(7.0, 4.3, 0.9);
            line(p(7.0, 6.4), p(7.0, 10.3));
        }
        IconKind::Play => {
            poly(&[(4.0, 2.2), (11.8, 7.0), (4.0, 11.8)], true);
        }
        IconKind::Refresh => {
            arc(7.0, 7.0, 4.8, -60.0, 230.0);
            poly(&[(9.4, 0.9), (9.4, 2.9), (11.4, 3.3)], false);
        }
        IconKind::Recycle => {
            arc(7.0, 7.0, 4.8, 200.0, 340.0);
            arc(7.0, 7.0, 4.8, 20.0, 160.0);
            poly(&[(10.3, 1.9), (11.5, 5.4), (8.1, 5.2)], false);
            poly(&[(3.7, 12.1), (2.5, 8.6), (5.9, 8.8)], false);
        }
        IconKind::Plug => {
            line(p(5.0, 1.0), p(5.0, 4.0));
            line(p(9.0, 1.0), p(9.0, 4.0));
            rect_at(3.0, 4.0, 11.0, 8.6, 2.0);
            line(p(7.0, 8.6), p(7.0, 13.0));
        }
        IconKind::Lock | IconKind::Unlock => {
            rect_at(2.5, 6.5, 11.5, 13.0, 1.5);
            if icon == IconKind::Lock {
                arc(7.0, 5.0, 2.8, 180.0, 360.0);
                line(p(4.2, 5.0), p(4.2, 6.5));
                line(p(9.8, 5.0), p(9.8, 6.5));
            } else {
                arc(9.8, 4.0, 2.8, 180.0, 360.0);
                line(p(7.0, 4.0), p(7.0, 6.5));
            }
            line(p(7.0, 9.0), p(7.0, 10.6));
        }
        IconKind::Folder => {
            poly(
                &[
                    (1.5, 2.5),
                    (5.5, 2.5),
                    (7.0, 4.2),
                    (12.5, 4.2),
                    (12.5, 11.8),
                    (1.5, 11.8),
                ],
                true,
            );
        }
        IconKind::Pin => {
            line(p(4.0, 1.5), p(10.0, 1.5));
            line(p(5.5, 1.5), p(5.0, 7.5));
            line(p(8.5, 1.5), p(9.0, 7.5));
            line(p(3.0, 7.5), p(11.0, 7.5));
            line(p(7.0, 7.5), p(7.0, 13.0));
        }
        IconKind::Star => {
            let points: Vec<(f32, f32)> = (0..10)
                .map(|step| {
                    let radius = if step % 2 == 0 { 6.0 } else { 2.6 };
                    let angle = (-90.0 + step as f32 * 36.0_f32).to_radians();
                    (7.0 + radius * angle.cos(), 7.4 + radius * angle.sin())
                })
                .collect();
            poly(&points, true);
        }
        IconKind::Monitor => {
            rect_at(1.5, 2.0, 12.5, 9.5, 1.0);
            line(p(7.0, 9.5), p(7.0, 12.0));
            line(p(4.5, 12.5), p(9.5, 12.5));
        }
        IconKind::Palette => {
            arc(7.0, 7.0, 5.8, 60.0, 350.0);
            poly(
                &[(12.7, 6.0), (10.0, 7.6), (10.4, 10.6), (9.9, 12.0)],
                false,
            );
            dot(4.4, 6.0, 0.9);
            dot(6.6, 3.7, 0.9);
            dot(9.4, 4.4, 0.9);
        }
        IconKind::Bolt => {
            poly(
                &[
                    (8.2, 1.0),
                    (3.0, 8.0),
                    (6.8, 8.0),
                    (5.8, 13.0),
                    (11.0, 6.0),
                    (7.2, 6.0),
                ],
                true,
            );
        }
        IconKind::Bulb => {
            arc(7.0, 5.6, 4.0, 140.0, 400.0);
            line(p(4.6, 8.8), p(5.2, 10.2));
            line(p(9.4, 8.8), p(8.8, 10.2));
            line(p(5.2, 10.4), p(8.8, 10.4));
            line(p(5.8, 12.6), p(8.2, 12.6));
        }
        IconKind::Image => {
            rect_at(1.5, 2.5, 12.5, 11.5, 1.5);
            poly(
                &[
                    (2.5, 10.5),
                    (5.5, 7.0),
                    (8.0, 9.2),
                    (9.5, 7.8),
                    (11.8, 10.2),
                ],
                false,
            );
            circle(9.3, 5.2, 1.0);
        }
        IconKind::Note => {
            poly(
                &[
                    (2.5, 1.5),
                    (8.5, 1.5),
                    (11.5, 4.5),
                    (11.5, 12.5),
                    (2.5, 12.5),
                ],
                true,
            );
            line(p(4.8, 6.5), p(9.2, 6.5));
            line(p(4.8, 9.2), p(9.2, 9.2));
        }
        IconKind::Treemap => {
            rect_at(1.5, 1.5, 12.5, 12.5, 1.5);
            line(p(7.5, 1.5), p(7.5, 12.5));
            line(p(1.5, 8.0), p(7.5, 8.0));
            line(p(7.5, 5.5), p(12.5, 5.5));
            line(p(10.0, 5.5), p(10.0, 12.5));
        }
        IconKind::Ranking => {
            for (y, width) in [(3.0, 11.0), (7.0, 8.0), (11.0, 5.0)] {
                line(p(1.5, y), p(1.5 + width, y));
            }
        }
        IconKind::PieChart => {
            circle(7.0, 7.0, 5.8);
            line(p(7.0, 7.0), p(7.0, 1.2));
            line(p(7.0, 7.0), p(12.2, 9.6));
        }
        IconKind::Check => {
            poly(&[(2.2, 7.4), (5.6, 10.8), (11.8, 3.6)], false);
        }
        IconKind::Warning => {
            poly(&[(7.0, 1.2), (13.0, 12.5), (1.0, 12.5)], true);
            line(p(7.0, 5.2), p(7.0, 8.6));
            dot(7.0, 10.6, 0.8);
        }
        IconKind::Shield => {
            poly(
                &[
                    (7.0, 1.0),
                    (12.0, 3.0),
                    (11.4, 8.2),
                    (7.0, 13.0),
                    (2.6, 8.2),
                    (2.0, 3.0),
                ],
                true,
            );
        }
        IconKind::Plus => {
            line(p(7.0, 2.5), p(7.0, 11.5));
            line(p(2.5, 7.0), p(11.5, 7.0));
        }
        IconKind::Back => {
            line(p(12.0, 7.0), p(2.5, 7.0));
            poly(&[(6.2, 3.2), (2.5, 7.0), (6.2, 10.8)], false);
        }
        IconKind::ChevronDown => poly(&[(3.5, 5.2), (7.0, 8.8), (10.5, 5.2)], false),
        IconKind::ChevronRight => poly(&[(5.2, 3.5), (8.8, 7.0), (5.2, 10.5)], false),
        IconKind::Copy => {
            rect_at(4.5, 4.5, 12.5, 12.5, 1.5);
            poly(
                &[(2.0, 9.5), (1.5, 9.5), (1.5, 1.5), (9.5, 1.5), (9.5, 2.0)],
                false,
            );
        }
        IconKind::Trash => {
            line(p(2.0, 3.5), p(12.0, 3.5));
            poly(&[(5.0, 3.5), (5.0, 1.5), (9.0, 1.5), (9.0, 3.5)], false);
            poly(&[(3.0, 3.5), (3.8, 12.5), (10.2, 12.5), (11.0, 3.5)], false);
            line(p(5.5, 5.5), p(5.8, 10.5));
            line(p(8.5, 5.5), p(8.2, 10.5));
        }
        IconKind::Puzzle => {
            rect_at(2.0, 4.0, 10.0, 12.0, 1.0);
            circle(6.0, 2.6, 1.4);
            circle(11.4, 8.0, 1.4);
        }
    }
}

/// Allocates a 14×14 icon slot in the current layout.
pub fn icon(ui: &mut Ui, icon: IconKind, color: Color32) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
    if ui.is_rect_visible(rect) {
        draw_icon(ui.painter(), rect, icon, color);
    }
    response
}

/// Downward chevron for every `ComboBox`; egui's default flips to ▲ when the list opens upward.
pub fn combo_icon(
    ui: &Ui,
    rect: Rect,
    visuals: &egui::style::WidgetVisuals,
    _open: bool,
    _above_or_below: egui::AboveOrBelow,
) {
    let center = rect.center();
    let color = visuals.fg_stroke.color;
    ui.painter().add(Shape::line(
        vec![
            center + vec2(-4.0, -2.0),
            center + vec2(0.0, 2.0),
            center + vec2(4.0, -2.0),
        ],
        stroke(1.5, color),
    ));
}

/// 1px outline around borderless popup windows, following the DWM rounded corners.
pub fn window_outline(context: &Context) {
    context
        .layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            Id::new("orbment.window_outline"),
        ))
        .rect_stroke(
            context.screen_rect(),
            CornerRadius::same(8),
            stroke(1.0, CARD_BORDER),
            StrokeKind::Inside,
        );
}

/// Must run after `SharedFonts::install`, which registers the bold heading family.
pub fn apply_fluent_dark(context: &Context) {
    context.set_theme(egui::Theme::Light);
    let bold = FontFamily::Name(BOLD_FAMILY.into());
    context.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Small, FontId::proportional(12.0)),
            (TextStyle::Body, FontId::proportional(14.0)),
            (TextStyle::Button, FontId::proportional(14.0)),
            (TextStyle::Heading, FontId::new(20.0, bold.clone())),
            (TextStyle::Monospace, FontId::monospace(13.0)),
        ]
        .into();
        let spacing = &mut style.spacing;
        spacing.item_spacing = vec2(10.0, 8.0);
        spacing.button_padding = vec2(12.0, 6.0);
        spacing.interact_size = vec2(44.0, 28.0);
        spacing.window_margin = Margin::same(16);
        spacing.menu_margin = Margin::same(8);
        spacing.combo_width = COMBO_WIDTH;
        spacing.text_edit_width = TEXT_WIDTH;
        spacing.slider_width = 180.0;
        spacing.icon_width = 16.0;
        spacing.icon_width_inner = 9.0;
        spacing.indent = 18.0;
        spacing.scroll = egui::style::ScrollStyle::floating();
        spacing.scroll.floating = true;
        spacing.scroll.bar_width = 6.0;
        spacing.scroll.floating_width = 6.0;
        spacing.scroll.handle_min_length = 28.0;
        spacing.scroll.bar_inner_margin = 6.0;
        spacing.scroll.bar_outer_margin = 2.0;
        spacing.scroll.floating_allocated_width = 0.0;
        spacing.scroll.foreground_color = false;
        spacing.scroll.dormant_handle_opacity = 0.0;
        spacing.scroll.active_handle_opacity = 0.32;
        spacing.scroll.interact_handle_opacity = 0.52;
        spacing.scroll.dormant_background_opacity = 0.0;
        spacing.scroll.active_background_opacity = 0.0;
        spacing.scroll.interact_background_opacity = 0.08;

        let visuals = &mut style.visuals;
        *visuals = egui::Visuals::light();
        visuals.panel_fill = BG;
        visuals.window_fill = BG;
        visuals.window_stroke = stroke(1.0, CARD_BORDER);
        visuals.window_corner_radius = CornerRadius::same(10);
        visuals.menu_corner_radius = CornerRadius::same(8);
        visuals.window_shadow = Shadow {
            offset: [0, 8],
            blur: 24,
            spread: 0,
            color: Color32::from_black_alpha(28),
        };
        visuals.popup_shadow = Shadow {
            offset: [0, 4],
            blur: 14,
            spread: 0,
            color: Color32::from_black_alpha(28),
        };
        visuals.extreme_bg_color = CONTROL;
        visuals.faint_bg_color = CARD;
        visuals.code_bg_color = CONTROL;
        visuals.hyperlink_color = ACCENT_LIGHT;
        visuals.warn_fg_color = WARNING;
        visuals.error_fg_color = DANGER;
        visuals.selection.bg_fill = ACCENT_SOFT;
        visuals.selection.stroke = stroke(1.0, ACCENT_LIGHT);
        visuals.text_cursor.stroke = stroke(2.0, ACCENT_LIGHT);
        visuals.slider_trailing_fill = true;
        visuals.collapsing_header_frame = false;
        visuals.indent_has_left_vline = false;
        visuals.striped = false;

        let widgets = &mut visuals.widgets;
        widgets.noninteractive.bg_fill = CARD;
        widgets.noninteractive.weak_bg_fill = CARD;
        widgets.noninteractive.bg_stroke = stroke(1.0, NAV_DIVIDER);
        widgets.noninteractive.fg_stroke = stroke(1.0, TEXT);
        widgets.noninteractive.corner_radius = CornerRadius::same(6);
        for (state, fill, border, text) in [
            (&mut widgets.inactive, CONTROL, CONTROL_BORDER, TEXT),
            (
                &mut widgets.hovered,
                CONTROL_HOVER,
                CONTROL_HOVER_BORDER,
                TEXT,
            ),
            (&mut widgets.active, ACCENT_SOFT, ACCENT_LIGHT, TEXT),
            (&mut widgets.open, CONTROL_HOVER, CONTROL_HOVER_BORDER, TEXT),
        ] {
            state.bg_fill = fill;
            state.weak_bg_fill = fill;
            state.bg_stroke = stroke(1.0, border);
            state.fg_stroke = stroke(1.5, text);
            state.corner_radius = CornerRadius::same(6);
            state.expansion = 0.0;
        }
    });
}

pub fn bold_family(context: &Context) -> FontFamily {
    let family = FontFamily::Name(BOLD_FAMILY.into());
    if context.fonts(|fonts| fonts.families().contains(&family)) {
        family
    } else {
        FontFamily::Proportional
    }
}

pub fn bold(context: &Context, size: f32) -> FontId {
    FontId::new(size, bold_family(context))
}

pub fn strong(ui: &Ui, text: impl Into<String>, size: f32) -> RichText {
    RichText::new(text).font(bold(ui.ctx(), size)).color(TEXT)
}

pub fn muted(text: impl Into<String>) -> RichText {
    RichText::new(text).size(12.0).color(TEXT_MUTED)
}

pub fn faint(text: impl Into<String>) -> RichText {
    RichText::new(text).size(12.0).color(TEXT_FAINT)
}

/// Keep page content clear of the floating scrollbar; apply inside scroll areas.
pub(crate) fn page_content_frame() -> Frame {
    Frame::new().inner_margin(Margin {
        right: 24,
        ..Default::default()
    })
}

pub fn card_frame() -> Frame {
    Frame::new()
        .fill(CARD)
        .stroke(stroke(1.0, CARD_BORDER))
        .corner_radius(8)
        .inner_margin(Margin::same(14))
}

/// Rounded list entry used for ports, file locks and residual processes.
pub fn list_row_frame() -> Frame {
    Frame::new()
        .fill(CARD)
        .stroke(stroke(1.0, CARD_BORDER))
        .corner_radius(8)
        .inner_margin(Margin::symmetric(14, 10))
}

/// Open section with a heading and hairline dividers between its rows.
pub fn section_card<R>(ui: &mut Ui, title: Option<&str>, add: impl FnOnce(&mut Ui) -> R) -> R {
    if let Some(title) = title {
        ui.add_space(10.0);
        ui.label(strong(ui, title, 15.5));
        ui.add_space(6.0);
    }
    let key = Id::new(CARD_ROWS);
    let outer = ui.data(|data| data.get_temp::<usize>(key));
    ui.data_mut(|data| data.insert_temp(key, 0usize));
    let result = Frame::new()
        .inner_margin(Margin::symmetric(2, 4))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner;
    ui.data_mut(|data| match outer {
        Some(count) => data.insert_temp(key, count),
        None => data.remove::<usize>(key),
    });
    ui.add_space(14.0);
    result
}

/// Two-column form row: label on the left, controls right-aligned.
pub fn setting_row<R>(
    ui: &mut Ui,
    label: impl Into<WidgetText>,
    add_controls: impl FnOnce(&mut Ui) -> R,
) -> R {
    icon_setting_row(ui, None, label, add_controls)
}

/// Compact aligned row with a secondary hint; long hints remain available on hover.
pub fn setting_row_sub<R>(
    ui: &mut Ui,
    label: impl Into<String>,
    subtitle: impl Into<String>,
    add_controls: impl FnOnce(&mut Ui) -> R,
) -> R {
    let label_width = (ui.available_width() * 0.36).clamp(180.0, 320.0);
    let label_height =
        ui.text_style_height(&TextStyle::Body) + ui.text_style_height(&TextStyle::Small) + 2.0;
    let subtitle = subtitle.into();
    setting_row_with_label(
        ui,
        |ui| {
            ui.allocate_ui_with_layout(
                vec2(label_width, label_height),
                Layout::top_down(Align::Min),
                |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.add(egui::Label::new(row_label(label)).truncate());
                    ui.add(egui::Label::new(faint(&subtitle)).truncate())
                        .on_hover_text(&subtitle);
                },
            );
        },
        add_controls,
    )
}

/// [`setting_row`] with an optional leading line icon.
pub fn icon_setting_row<R>(
    ui: &mut Ui,
    leading: Option<IconKind>,
    label: impl Into<WidgetText>,
    add_controls: impl FnOnce(&mut Ui) -> R,
) -> R {
    setting_row_with_label(
        ui,
        |ui| {
            if let Some(leading) = leading {
                icon(ui, leading, TEXT_MUTED);
            }
            ui.add(egui::Label::new(label.into()).selectable(false));
        },
        add_controls,
    )
}

fn setting_row_with_label<R>(
    ui: &mut Ui,
    add_label: impl FnOnce(&mut Ui),
    add_controls: impl FnOnce(&mut Ui) -> R,
) -> R {
    let key = Id::new(CARD_ROWS);
    let index = ui.data(|data| data.get_temp::<usize>(key));
    let width = ui.available_width();
    let gap = ui.spacing().item_spacing.y;
    let inner = ui.allocate_ui_with_layout(
        vec2(width, ROW_HEIGHT),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.set_min_size(vec2(width, ROW_HEIGHT));
            add_label(ui);
            ui.with_layout(Layout::right_to_left(Align::Center), add_controls)
                .inner
        },
    );
    if let Some(index) = index {
        if index > 0 {
            let rect = inner.response.rect;
            let y = rect.top() - gap * 0.5;
            ui.painter()
                .hline(rect.left()..=rect.right(), y, stroke(1.0, ROW_DIVIDER));
        }
        ui.data_mut(|data| data.insert_temp(key, index + 1));
    }
    inner.inner
}

/// Free-form section content that participates in the row divider sequence.
pub fn setting_block<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    let key = Id::new(CARD_ROWS);
    let index = ui.data(|data| data.get_temp::<usize>(key));
    let gap = ui.spacing().item_spacing.y;
    let inner = ui.vertical(|ui| {
        ui.add_space(4.0);
        let result = add(ui);
        ui.add_space(4.0);
        result
    });
    if let Some(index) = index {
        if index > 0 {
            let rect = inner.response.rect;
            ui.painter().hline(
                rect.left()..=ui.max_rect().right(),
                rect.top() - gap * 0.5,
                stroke(1.0, ROW_DIVIDER),
            );
        }
        ui.data_mut(|data| data.insert_temp(key, index + 1));
    }
    inner.inner
}

pub fn row_label(text: impl Into<String>) -> RichText {
    RichText::new(text).size(14.0).color(TEXT)
}

pub fn toggle_switch(ui: &mut Ui, on: &mut bool) -> Response {
    toggle_switch_sized(ui, on, vec2(38.0, 21.0))
}

/// Rounded capsule switch; replaces raw checkboxes for every boolean.
pub fn toggle_switch_sized(ui: &mut Ui, on: &mut bool, size: Vec2) -> Response {
    let (rect, mut response) = ui.allocate_exact_size(size, Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), *on, "")
    });
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_responsive(response.id, *on);
        let hovered = response.hovered();
        let off_fill = if hovered {
            Color32::from_rgb(214, 211, 203)
        } else {
            Color32::from_rgb(225, 222, 215)
        };
        let on_fill = if hovered {
            Color32::from_rgb(100, 138, 180)
        } else {
            Color32::from_rgb(116, 152, 192)
        };
        let fill = lerp_color(off_fill, on_fill, t);
        let border = lerp_color(Color32::from_rgb(208, 204, 196), on_fill, t);
        let radius = rect.height() * 0.5;
        ui.painter().rect(
            rect,
            CornerRadius::same(radius as u8),
            fill,
            stroke(1.0, border),
            StrokeKind::Inside,
        );
        let knob = (radius - 2.5).max(4.0);
        let x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), t);
        ui.painter().circle_filled(
            egui::pos2(x, rect.center().y + 0.8),
            knob,
            Color32::from_black_alpha(18),
        );
        ui.painter()
            .circle_filled(egui::pos2(x, rect.center().y), knob, Color32::WHITE);
        if response.has_focus() {
            ui.painter().rect_stroke(
                rect.expand(2.0),
                CornerRadius::same(radius as u8 + 2),
                stroke(1.0, ACCENT_LIGHT),
                StrokeKind::Outside,
            );
        }
    }
    response
}

pub fn lerp_color(from: Color32, to: Color32, t: f32) -> Color32 {
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t.clamp(0.0, 1.0)).round() as u8;
    Color32::from_rgba_unmultiplied(
        mix(from.r(), to.r()),
        mix(from.g(), to.g()),
        mix(from.b(), to.b()),
        mix(from.a(), to.a()),
    )
}

/// Single-line input padded to the 28px control height used by combos and number fields.
pub fn text_input(text: &mut dyn egui::TextBuffer) -> egui::TextEdit<'_> {
    egui::TextEdit::singleline(text).margin(Margin::symmetric(8, 5))
}

/// Small non-interactive capsule badge.
pub fn pill(ui: &mut Ui, text: impl Into<String>, fg: Color32, bg: Color32) -> Response {
    badge(ui, text, 12.0, fg, bg, vec2(8.0, 2.0), 9)
}

/// Painted badge sized to its text, independent of the surrounding row height.
pub fn badge(
    ui: &mut Ui,
    text: impl Into<String>,
    size: f32,
    fg: Color32,
    bg: Color32,
    padding: Vec2,
    radius: u8,
) -> Response {
    let galley = ui
        .painter()
        .layout_no_wrap(text.into(), FontId::proportional(size), fg);
    let (rect, response) = ui.allocate_exact_size(galley.size() + padding * 2.0, Sense::hover());
    if ui.is_rect_visible(rect) {
        ui.painter().rect_filled(rect, radius, bg);
        ui.painter().galley(rect.min + padding, galley, fg);
    }
    response
}

/// [`pill`] with a leading 12px line icon.
pub fn icon_pill(
    ui: &mut Ui,
    icon: IconKind,
    text: impl Into<String>,
    fg: Color32,
    bg: Color32,
) -> Response {
    let galley = ui
        .painter()
        .layout_no_wrap(text.into(), FontId::proportional(12.0), fg);
    let padding = vec2(8.0, 2.0);
    let icon_size = 12.0;
    let size = vec2(
        icon_size + 5.0 + galley.size().x,
        galley.size().y.max(icon_size),
    ) + padding * 2.0;
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    if ui.is_rect_visible(rect) {
        ui.painter().rect_filled(rect, 9, bg);
        draw_icon(
            ui.painter(),
            Rect::from_center_size(
                pos2(rect.left() + padding.x + icon_size * 0.5, rect.center().y),
                Vec2::splat(icon_size),
            ),
            icon,
            fg,
        );
        ui.painter().galley(
            pos2(
                rect.left() + padding.x + icon_size + 5.0,
                rect.center().y - galley.size().y * 0.5,
            ),
            galley,
            fg,
        );
    }
    response
}

/// Capsule toggle used for categories and preset chips.
pub fn chip(ui: &mut Ui, text: impl Into<String>, selected: bool) -> Response {
    ui.add(IconButton::new(
        if selected {
            ButtonKind::ChipSelected
        } else {
            ButtonKind::Chip
        },
        text,
    ))
}

pub fn icon_chip(ui: &mut Ui, icon: IconKind, text: impl Into<String>, selected: bool) -> Response {
    ui.add(
        IconButton::new(
            if selected {
                ButtonKind::ChipSelected
            } else {
                ButtonKind::Chip
            },
            text,
        )
        .icon(icon),
    )
}

/// Segmented capsule tab strip; returns the newly selected key.
pub fn segmented_tabs(
    ui: &mut Ui,
    current: &str,
    items: &[(&str, Option<IconKind>, String)],
) -> Option<String> {
    let mut clicked = None;
    Frame::new()
        .fill(Color32::from_rgb(239, 238, 233))
        .stroke(Stroke::NONE)
        .corner_radius(8)
        .inner_margin(Margin::same(3))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                for (key, icon, label) in items {
                    let selected = *key == current;
                    let mut button = IconButton::new(
                        if selected {
                            ButtonKind::TabSelected
                        } else {
                            ButtonKind::Tab
                        },
                        label.clone(),
                    );
                    if let Some(icon) = icon {
                        button = button.icon(*icon);
                    }
                    if ui.add(button).clicked() && !selected {
                        clicked = Some((*key).to_owned());
                    }
                }
            });
        });
    clicked
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonKind {
    Primary,
    Secondary,
    Danger,
    Small,
    Chip,
    ChipSelected,
    Tab,
    TabSelected,
}

struct ButtonStyle {
    fill: Color32,
    hover_fill: Color32,
    border: Color32,
    hover_border: Color32,
    text: Color32,
    icon: Color32,
    font_size: f32,
    height: f32,
    padding: f32,
    radius: u8,
}

fn button_style(kind: ButtonKind) -> ButtonStyle {
    let base = ButtonStyle {
        fill: CONTROL,
        hover_fill: CONTROL_HOVER,
        border: CONTROL_BORDER,
        hover_border: CONTROL_HOVER_BORDER,
        text: TEXT,
        icon: TEXT_MUTED,
        font_size: 14.0,
        height: 30.0,
        padding: 12.0,
        radius: 6,
    };
    match kind {
        ButtonKind::Primary => ButtonStyle {
            fill: ACCENT,
            hover_fill: ACCENT_HOVER,
            border: ACCENT,
            hover_border: ACCENT_HOVER,
            text: Color32::WHITE,
            icon: Color32::WHITE,
            ..base
        },
        ButtonKind::Secondary => base,
        ButtonKind::Danger => ButtonStyle {
            fill: DANGER_BG,
            hover_fill: Color32::from_rgb(254, 202, 202),
            border: Color32::from_rgb(252, 165, 165),
            hover_border: DANGER,
            text: DANGER,
            icon: DANGER,
            ..base
        },
        ButtonKind::Small => ButtonStyle {
            text: TEXT_MUTED,
            font_size: 12.0,
            height: 24.0,
            padding: 8.0,
            ..base
        },
        ButtonKind::Chip => ButtonStyle {
            fill: CARD,
            hover_fill: CARD_HOVER,
            border: CARD_BORDER,
            hover_border: CARD_HOVER_BORDER,
            text: TEXT_MUTED,
            font_size: 13.0,
            height: 28.0,
            radius: 14,
            ..base
        },
        ButtonKind::ChipSelected => ButtonStyle {
            fill: ACCENT_SOFT,
            hover_fill: CARD_SELECTED,
            border: CARD_HOVER_BORDER,
            hover_border: CONTROL_HOVER_BORDER,
            text: TEXT,
            icon: ACCENT_LIGHT,
            font_size: 13.0,
            height: 28.0,
            radius: 14,
            ..base
        },
        ButtonKind::Tab => ButtonStyle {
            fill: Color32::TRANSPARENT,
            hover_fill: NAV_HOVER,
            border: Color32::TRANSPARENT,
            hover_border: Color32::TRANSPARENT,
            text: TEXT_MUTED,
            font_size: 13.5,
            radius: 6,
            ..base
        },
        ButtonKind::TabSelected => ButtonStyle {
            fill: Color32::WHITE,
            hover_fill: Color32::WHITE,
            border: Color32::from_rgb(218, 216, 210),
            hover_border: CONTROL_HOVER_BORDER,
            text: TEXT,
            icon: ACCENT_LIGHT,
            font_size: 13.5,
            radius: 6,
            ..base
        },
    }
}

/// Painted button with an optional line icon; content is always centred in its frame,
/// independent of the parent layout's alignment.
#[must_use = "add the button with `ui.add`"]
pub struct IconButton {
    kind: ButtonKind,
    icon: Option<IconKind>,
    text: String,
    min_size: Option<Vec2>,
}

impl IconButton {
    pub fn new(kind: ButtonKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            icon: None,
            text: text.into(),
            min_size: None,
        }
    }

    pub fn icon(mut self, icon: IconKind) -> Self {
        self.icon = Some(icon);
        self
    }

    pub fn min_size(mut self, size: Vec2) -> Self {
        self.min_size = Some(size);
        self
    }
}

impl Widget for IconButton {
    fn ui(self, ui: &mut Ui) -> Response {
        let style = button_style(self.kind);
        let enabled = ui.is_enabled();
        let text_color = if enabled { style.text } else { TEXT_FAINT };
        let icon_color = if enabled { style.icon } else { TEXT_FAINT };
        let galley = ui.painter().layout_no_wrap(
            self.text.clone(),
            FontId::proportional(style.font_size),
            text_color,
        );
        let icon_width = match (self.icon, self.text.is_empty()) {
            (Some(_), true) => 14.0,
            (Some(_), false) => 20.0,
            (None, _) => 0.0,
        };
        let content = vec2(icon_width + galley.size().x, galley.size().y.max(14.0));
        let min = self.min_size.unwrap_or(vec2(0.0, style.height));
        let size = vec2(
            (content.x + style.padding * 2.0).max(min.x),
            min.y.max(content.y + 6.0),
        );
        let (rect, response) = ui.allocate_exact_size(size, Sense::click());
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), &self.text)
        });
        if ui.is_rect_visible(rect) {
            let hot = enabled && response.hovered();
            let mut fill = if !enabled {
                Color32::from_rgb(236, 234, 228)
            } else if hot {
                style.hover_fill
            } else {
                style.fill
            };
            if enabled && response.is_pointer_button_down_on() {
                fill = lerp_color(fill, Color32::BLACK, 0.12);
            }
            let border = if !enabled {
                CARD_BORDER
            } else if hot {
                style.hover_border
            } else {
                style.border
            };
            ui.painter().rect(
                rect,
                style.radius,
                fill,
                stroke(1.0, border),
                StrokeKind::Inside,
            );
            let left = rect.center().x - content.x * 0.5;
            if let Some(icon) = self.icon {
                draw_icon(
                    ui.painter(),
                    Rect::from_center_size(pos2(left + 7.0, rect.center().y), Vec2::splat(14.0)),
                    icon,
                    icon_color,
                );
            }
            ui.painter().galley(
                pos2(left + icon_width, rect.center().y - galley.size().y * 0.5),
                galley,
                text_color,
            );
            if response.has_focus() {
                ui.painter().rect_stroke(
                    rect.expand(2.0),
                    style.radius + 2,
                    stroke(1.0, ACCENT_LIGHT),
                    StrokeKind::Outside,
                );
            }
        }
        response
    }
}

pub fn primary_button(text: impl Into<String>) -> IconButton {
    IconButton::new(ButtonKind::Primary, text)
}

pub fn secondary_button(text: impl Into<String>) -> IconButton {
    IconButton::new(ButtonKind::Secondary, text)
}

pub fn danger_button(text: impl Into<String>) -> IconButton {
    IconButton::new(ButtonKind::Danger, text)
}

pub fn small_button(text: impl Into<String>) -> IconButton {
    IconButton::new(ButtonKind::Small, text)
}

/// Left text margin reserved for a leading input icon, which sits at `frame left + 8..22`.
pub const ICON_GUTTER: i8 = 30;

/// Shows a single-line edit whose margin starts with [`ICON_GUTTER`] and paints `icon` in that
/// gutter. The icon is placed relative to the text clip rect, because the response rect of a
/// `TextEdit` excludes its margin.
pub fn edit_with_icon(ui: &mut Ui, edit: egui::TextEdit<'_>, icon: IconKind) -> Response {
    let output = edit.show(ui);
    let frame_left = output.text_clip_rect.left() - f32::from(ICON_GUTTER);
    let center_y = output.text_clip_rect.center().y;
    draw_icon(
        ui.painter(),
        Rect::from_center_size(pos2(frame_left + 15.0, center_y), Vec2::splat(14.0)),
        icon,
        TEXT_FAINT,
    );
    output.response
}

/// Rounded search field with a painted leading magnifier.
pub fn search_field(
    ui: &mut Ui,
    text: &mut String,
    hint: &str,
    width: f32,
    height: f32,
) -> Response {
    let font = FontId::proportional(if height >= 40.0 { 15.0 } else { 14.0 });
    let vertical = ((height - font.size - 6.0) * 0.5).max(2.0) as i8;
    edit_with_icon(
        ui,
        egui::TextEdit::singleline(text)
            .hint_text(RichText::new(hint).color(TEXT_FAINT))
            .font(font)
            .margin(Margin {
                left: ICON_GUTTER,
                right: 10,
                top: vertical,
                bottom: vertical,
            })
            .desired_width(width),
        IconKind::Search,
    )
}

pub enum Tone {
    Info,
    Warning,
    Danger,
}

pub fn callout(ui: &mut Ui, tone: Tone, text: impl Into<String>) {
    let (fill, border, color) = match tone {
        Tone::Info => (CARD, CARD_BORDER, TEXT_MUTED),
        Tone::Warning => (WARNING_BG, Color32::from_rgb(253, 230, 138), WARNING),
        Tone::Danger => (DANGER_BG, Color32::from_rgb(252, 165, 165), DANGER),
    };
    Frame::new()
        .fill(fill)
        .stroke(stroke(1.0, border))
        .corner_radius(6)
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(egui::Label::new(RichText::new(text).size(12.5).color(color)).wrap());
        });
}

/// Inner rows: title 0..16, value 19..46, bar 47..53, detail anchored to the bottom (≈60..76),
/// leaving a clear gap between the bar and the detail line.
pub const STAT_CARD_HEIGHT: f32 = 100.0;

fn single_line(
    ui: &Ui,
    text: &str,
    font: FontId,
    color: Color32,
    width: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(width.max(1.0));
    ui.fonts(|fonts| fonts.layout_job(job))
}

/// Fixed-height metric tile with an optional usage bar; every row sits at a fixed offset so
/// neighbouring tiles line up exactly.
pub fn stat_card(
    ui: &mut Ui,
    width: f32,
    title: &str,
    value: &str,
    detail: &str,
    progress: Option<f32>,
) {
    let (rect, _) = ui.allocate_exact_size(vec2(width, STAT_CARD_HEIGHT), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let painter = ui.painter();
    painter.rect(rect, 8, CARD, stroke(1.0, CARD_BORDER), StrokeKind::Inside);
    let inner = rect.shrink2(vec2(14.0, 12.0));
    let title = single_line(
        ui,
        title,
        FontId::proportional(12.0),
        TEXT_MUTED,
        inner.width(),
    );
    painter.galley(inner.left_top(), title, TEXT_MUTED);
    let value = single_line(ui, value, bold(ui.ctx(), 20.0), TEXT, inner.width());
    painter.galley(inner.left_top() + vec2(0.0, 19.0), value, TEXT);
    if let Some(progress) = progress {
        let bar = Rect::from_min_size(inner.left_top() + vec2(0.0, 47.0), vec2(inner.width(), 6.0));
        painter.rect_filled(bar, 3, ACCENT_SOFT);
        let mut filled = bar;
        filled.set_width(bar.width() * progress.clamp(0.0, 1.0));
        let color = if progress >= 0.85 {
            DANGER
        } else if progress >= 0.7 {
            WARNING
        } else {
            ACCENT
        };
        painter.rect_filled(filled, 3, color);
    }
    let detail = single_line(
        ui,
        detail,
        FontId::proportional(12.0),
        TEXT_FAINT,
        inner.width(),
    );
    painter.galley(
        pos2(inner.left(), inner.bottom() - detail.size().y),
        detail,
        TEXT_FAINT,
    );
}

/// Page header: bold title plus a one-line description.
pub fn page_title(ui: &mut Ui, title: &str, description: &str) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.add(egui::Label::new(strong(ui, title, 20.0)).selectable(false));
        if !description.is_empty() {
            ui.add(egui::Label::new(muted(description)).selectable(false));
        }
    });
}

/// The picker is modal to the active Orbment window and never returns a virtual Shell path.
fn pick_path_dialog(initial: Option<&str>, folder: bool, exe_only: bool) -> Option<String> {
    use windows::{
        Win32::{
            System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree},
            System::Ole::{OleInitialize, OleUninitialize},
            UI::{
                Input::KeyboardAndMouse::GetActiveWindow,
                Shell::{
                    Common::COMDLG_FILTERSPEC, FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST,
                    FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, IShellItem,
                    SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
                },
            },
        },
        core::{PCWSTR, w},
    };
    struct Apartment;
    impl Drop for Apartment {
        fn drop(&mut self) {
            unsafe { OleUninitialize() };
        }
    }
    let result = (|| -> windows::core::Result<String> {
        unsafe {
            OleInitialize(None)?;
            let _apartment = Apartment;
            let dialog: IFileOpenDialog =
                CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
            let mut options = dialog.GetOptions()? | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST;
            if folder {
                options |= FOS_PICKFOLDERS;
            }
            dialog.SetOptions(options)?;
            if exe_only && !folder {
                dialog.SetFileTypes(&[
                    COMDLG_FILTERSPEC {
                        pszName: w!("应用程序 (*.exe)"),
                        pszSpec: w!("*.exe"),
                    },
                    COMDLG_FILTERSPEC {
                        pszName: w!("所有文件"),
                        pszSpec: w!("*.*"),
                    },
                ])?;
            }
            if let Some(initial) = initial.filter(|text| !text.is_empty()) {
                let path = std::path::Path::new(initial);
                let directory = if path.is_dir() {
                    Some(path)
                } else {
                    path.parent()
                };
                if let Some(directory) = directory.filter(|path| path.is_dir()) {
                    let wide: Vec<u16> = directory
                        .as_os_str()
                        .to_string_lossy()
                        .encode_utf16()
                        .chain(Some(0))
                        .collect();
                    let item: IShellItem =
                        SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None)?;
                    dialog.SetFolder(&item)?;
                }
            }
            let owner = GetActiveWindow();
            dialog.Show((!owner.is_invalid()).then_some(owner))?;
            let name = dialog.GetResult()?.GetDisplayName(SIGDN_FILESYSPATH)?;
            let text = name.to_string();
            CoTaskMemFree(Some(name.0.cast()));
            Ok(text?)
        }
    })();
    match result {
        Ok(path) => Some(path),
        Err(error) => {
            // Cancellation is an expected result, not a configuration error.
            if error.code().0 as u32 != 0x800704c7 {
                tracing::warn!(%error, "Unable to open native path picker");
            }
            None
        }
    }
}

pub fn pick_folder_dialog(initial: Option<&str>) -> Option<String> {
    pick_path_dialog(initial, true, false)
}

pub fn pick_file_dialog(initial: Option<&str>, exe_only: bool) -> Option<String> {
    pick_path_dialog(initial, false, exe_only)
}

/// Compact icon-only control with a fixed hit target, so feedback never moves its neighbours.
pub fn icon_button(ui: &mut Ui, icon: IconKind, color: Color32, active: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(24.0, 24.0), Sense::click());
    let fill = if active {
        ACCENT_SOFT
    } else if response.hovered() {
        CONTROL_HOVER
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect(
        rect,
        5,
        fill,
        stroke(1.0, if active { color } else { Color32::TRANSPARENT }),
        egui::StrokeKind::Inside,
    );
    draw_icon(
        ui.painter(),
        rect.shrink(if response.hovered() { 4.0 } else { 5.0 }),
        icon,
        color,
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_primary_button_lays_out_readable_text() {
        let context = egui::Context::default();
        let output = context.run(egui::RawInput::default(), |context| {
            egui::CentralPanel::default().show(context, |ui| {
                let response = ui.add_enabled(false, primary_button("Save key"));
                assert!(!response.enabled());
            });
        });
        let text = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == "Save key" => Some(text),
                _ => None,
            })
            .expect("disabled button label must remain visible");
        assert!(
            text.galley
                .job
                .sections
                .iter()
                .all(|section| section.format.color == TEXT_FAINT)
        );
    }
}
