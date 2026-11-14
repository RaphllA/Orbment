use super::{InputEvent, PointerAction, PointerButton, TICKS_PER_SECOND, TimedEvent};
use crate::CapturedImage;
use anyhow::{Result, ensure};

#[derive(Clone, Debug)]
pub struct EffectOptions {
    pub show_pointer: bool,
    pub pointer_scale: f64,
    pub smooth_pointer: bool,
    pub simple_pointer: bool,
    pub animation_speed: f64,
    pub left_color: [u8; 4],
    pub right_color: [u8; 4],
    pub middle_color: [u8; 4],
    pub show_characters: bool,
    pub show_combinations: bool,
    pub show_modifiers: bool,
    pub show_function_keys: bool,
    pub key_font_size: u32,
    pub key_text_color: [u8; 4],
    pub key_background_color: [u8; 4],
    pub key_stay_100ns: i64,
    pub character_merge_limit: usize,
    pub key_history_rows: usize,
    pub system_audio_gain: f32,
    pub microphone_gain: f32,
}
impl Default for EffectOptions {
    fn default() -> Self {
        Self {
            show_pointer: true,
            pointer_scale: 1.0,
            smooth_pointer: true,
            simple_pointer: false,
            animation_speed: 1.0,
            left_color: [70, 150, 255, 210],
            right_color: [255, 100, 80, 210],
            middle_color: [90, 220, 130, 210],
            show_characters: true,
            show_combinations: true,
            show_modifiers: true,
            show_function_keys: true,
            key_font_size: 24,
            key_text_color: [255, 255, 255, 255],
            key_background_color: [25, 25, 30, 200],
            key_stay_100ns: 20_000_000,
            character_merge_limit: 24,
            key_history_rows: 4,
            system_audio_gain: 1.0,
            microphone_gain: 1.0,
        }
    }
}
impl EffectOptions {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.pointer_scale.is_finite() && (0.1..=10.0).contains(&self.pointer_scale),
            "invalid pointer scale"
        );
        ensure!(
            self.animation_speed.is_finite() && (0.1..=10.0).contains(&self.animation_speed),
            "invalid animation speed"
        );
        ensure!(
            (8..=128).contains(&self.key_font_size)
                && (0..=600_000_000).contains(&self.key_stay_100ns),
            "invalid key bubble size or duration"
        );
        ensure!(
            self.character_merge_limit <= 256 && self.key_history_rows <= 32,
            "key bubble history limits exceeded"
        );
        ensure!(
            self.system_audio_gain.is_finite() && self.microphone_gain.is_finite(),
            "nonfinite audio gain"
        );
        Ok(())
    }
}
impl From<&orbment_core::config::VisionKeyMouseConfig> for EffectOptions {
    fn from(config: &orbment_core::config::VisionKeyMouseConfig) -> Self {
        Self {
            show_pointer: config.record_mouse,
            pointer_scale: config.pointer_scale,
            smooth_pointer: config.smooth_track,
            simple_pointer: config.simplify_animation,
            animation_speed: config.animation_speed,
            left_color: config.left_color,
            right_color: config.right_color,
            middle_color: config.middle_color,
            show_characters: config.record_keyboard && config.show_characters,
            show_combinations: config.record_keyboard && config.show_combinations,
            show_modifiers: config.record_keyboard && config.show_modifiers,
            show_function_keys: config.record_keyboard && config.show_function_keys,
            key_font_size: config.key_font_size,
            key_text_color: config.key_text_color,
            key_background_color: config.key_background_color,
            key_stay_100ns: i64::from(config.key_keep_time_ms) * 10_000,
            character_merge_limit: config.character_merge_limit as usize,
            key_history_rows: config.max_history_count as usize,
            ..Self::default()
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyCategory {
    Character,
    Combination,
    Modifier,
    Function,
}
#[derive(Clone, Debug, PartialEq)]
pub struct KeyBubble {
    pub text: String,
    pub category: KeyCategory,
    pub opacity: f64,
    pub time_100ns: i64,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointerVisual {
    pub x: f64,
    pub y: f64,
}
#[derive(Clone, Copy, Debug)]
pub struct Ripple {
    pub x: f64,
    pub y: f64,
    pub progress: f64,
    pub button: PointerButton,
}
#[derive(Clone, Copy, Debug)]
pub struct WheelVisual {
    pub x: f64,
    pub y: f64,
    pub horizontal: bool,
    pub positive: bool,
    pub opacity: f64,
}
#[derive(Clone, Debug, Default)]
pub struct VisualFrame {
    pub pointer: Option<PointerVisual>,
    pub ripples: Vec<Ripple>,
    pub wheel: Option<WheelVisual>,
    pub keys: Vec<KeyBubble>,
}

#[derive(Clone, Debug, Default)]
pub struct Timeline {
    events: Vec<TimedEvent>,
    pointers: Vec<(i64, PointerVisual)>,
}
impl Timeline {
    pub fn new(mut events: Vec<TimedEvent>) -> Self {
        events.sort_by_key(|event| event.time_100ns);
        let mut held_modifiers = std::collections::HashSet::new();
        events.retain(|event| match event.event {
            InputEvent::Key {
                virtual_key,
                down: true,
                modifiers,
                ..
            } if key_category(virtual_key, modifiers) == KeyCategory::Modifier => {
                held_modifiers.insert(virtual_key)
            }
            InputEvent::Key {
                virtual_key,
                down: false,
                ..
            } => {
                held_modifiers.remove(&virtual_key);
                true
            }
            _ => true,
        });
        let pointers = events
            .iter()
            .filter_map(|event| match event.event {
                InputEvent::Pointer { x, y, .. } if x.is_finite() && y.is_finite() => {
                    Some((event.time_100ns, PointerVisual { x, y }))
                }
                _ => None,
            })
            .collect();
        Self { events, pointers }
    }
    pub fn events(&self) -> &[TimedEvent] {
        &self.events
    }
    pub fn at(&self, time: i64, options: &EffectOptions) -> VisualFrame {
        let mut frame = VisualFrame::default();
        if options.show_pointer {
            let index = self.pointers.partition_point(|(at, _)| *at <= time);
            if index > 0 {
                let (before, mut pointer) = self.pointers[index - 1];
                // Do not glide across a pause or a discontinuity in captured input.
                if options.smooth_pointer
                    && !options.simple_pointer
                    && let Some(&(after, next)) = self.pointers.get(index)
                    && after > before
                    && after - before <= TICKS_PER_SECOND / 4
                {
                    let fraction =
                        ((time - before) as f64 / (after - before) as f64).clamp(0.0, 1.0);
                    let smooth = fraction * fraction * (3.0 - 2.0 * fraction);
                    pointer.x += (next.x - pointer.x) * smooth;
                    pointer.y += (next.y - pointer.y) * smooth;
                }
                frame.pointer = Some(pointer);
            }
        }
        let ripple_duration = (6_000_000.0 / options.animation_speed) as i64;
        let key_start = time.saturating_sub(options.key_stay_100ns);
        let oldest = key_start.min(time.saturating_sub(ripple_duration));
        let start = self
            .events
            .partition_point(|event| event.time_100ns < oldest);
        let end = self
            .events
            .partition_point(|event| event.time_100ns <= time);
        for event in &self.events[start..end] {
            match event.event {
                InputEvent::Pointer { x, y, action }
                    if options.show_pointer && !options.simple_pointer =>
                {
                    let progress = (time - event.time_100ns) as f64 / ripple_duration as f64;
                    if !(0.0..1.0).contains(&progress) {
                        continue;
                    }
                    match action {
                        PointerAction::Down(button) => frame.ripples.push(Ripple {
                            x,
                            y,
                            progress,
                            button,
                        }),
                        PointerAction::Wheel { delta, horizontal } => {
                            frame.wheel = Some(WheelVisual {
                                x,
                                y,
                                horizontal,
                                positive: delta > 0,
                                opacity: 1.0 - progress,
                            })
                        }
                        _ => {}
                    }
                }
                InputEvent::Key {
                    virtual_key,
                    down: true,
                    modifiers,
                    ..
                } if event.time_100ns >= key_start => {
                    let category = key_category(virtual_key, modifiers);
                    let enabled = match category {
                        KeyCategory::Character => options.show_characters,
                        KeyCategory::Combination => options.show_combinations,
                        KeyCategory::Modifier => options.show_modifiers,
                        KeyCategory::Function => options.show_function_keys,
                    };
                    if !enabled {
                        continue;
                    }
                    let text = key_label(virtual_key, modifiers, category);
                    let age = (time - event.time_100ns) as f64;
                    let opacity =
                        ((options.key_stay_100ns as f64 - age) / 2_000_000.0).clamp(0.0, 1.0);
                    if category == KeyCategory::Character
                        && options.character_merge_limit > 0
                        && let Some(last) = frame.keys.last_mut()
                        && last.category == category
                        && event.time_100ns - last.time_100ns <= 4_000_000
                        && last.text.chars().count() + text.chars().count()
                            <= options.character_merge_limit
                    {
                        last.text.push_str(&text);
                        last.time_100ns = event.time_100ns;
                        last.opacity = opacity;
                        continue;
                    }
                    frame.keys.push(KeyBubble {
                        text,
                        category,
                        opacity,
                        time_100ns: event.time_100ns,
                    });
                }
                _ => {}
            }
        }
        let extra = frame.keys.len().saturating_sub(options.key_history_rows);
        frame.keys.drain(..extra);
        frame
    }
}

fn key_category(key: u32, modifiers: u8) -> KeyCategory {
    if matches!(key, 0x10..=0x12 | 0x5B | 0x5C | 0xA0..=0xA5) {
        KeyCategory::Modifier
    } else if modifiers & (1 | 4 | 8) != 0 {
        KeyCategory::Combination
    } else if matches!(key, 0x20 | 0x30..=0x5A | 0x60..=0x6F | 0xBA..=0xE2) {
        KeyCategory::Character
    } else {
        KeyCategory::Function
    }
}
fn key_label(key: u32, modifiers: u8, category: KeyCategory) -> String {
    let base = match key {
        0x30..=0x39 | 0x41..=0x5A => char::from_u32(key).unwrap_or('?').to_string(),
        0x60..=0x69 => format!("{}", key - 0x60),
        0x70..=0x87 => format!("F{}", key - 0x6F),
        0x08 => "Backspace".into(),
        0x09 => "Tab".into(),
        0x0D => "Enter".into(),
        0x10 | 0xA0 | 0xA1 => "Shift".into(),
        0x11 | 0xA2 | 0xA3 => "Ctrl".into(),
        0x12 | 0xA4 | 0xA5 => "Alt".into(),
        0x5B | 0x5C => "Win".into(),
        0x1B => "Esc".into(),
        0x20 => " ".into(),
        0x21 => "Page Up".into(),
        0x22 => "Page Down".into(),
        0x23 => "End".into(),
        0x24 => "Home".into(),
        0x25 => "←".into(),
        0x26 => "↑".into(),
        0x27 => "→".into(),
        0x28 => "↓".into(),
        0x2D => "Insert".into(),
        0x2E => "Delete".into(),
        _ => native_key_label(key),
    };
    if category == KeyCategory::Combination {
        let mut text = String::new();
        for (mask, name) in [(1, "Ctrl+"), (2, "Shift+"), (4, "Alt+"), (8, "Win+")] {
            if modifiers & mask != 0 {
                text.push_str(name);
            }
        }
        text.push_str(&base);
        text
    } else {
        base
    }
}
#[cfg(windows)]
fn native_key_label(key: u32) -> String {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetKeyNameTextW, MAPVK_VK_TO_VSC, MapVirtualKeyW,
    };
    let mut label = [0u16; 128];
    let length = unsafe {
        GetKeyNameTextW(
            (MapVirtualKeyW(key, MAPVK_VK_TO_VSC) << 16) as i32,
            &mut label,
        )
    };
    if length > 0 {
        String::from_utf16_lossy(&label[..length as usize])
    } else {
        format!("VK {key:02X}")
    }
}
#[cfg(not(windows))]
fn native_key_label(key: u32) -> String {
    format!("VK {key:02X}")
}

pub fn compose(
    image: &mut CapturedImage,
    frame: &VisualFrame,
    options: &EffectOptions,
) -> Result<()> {
    for ripple in &frame.ripples {
        let color = match ripple.button {
            PointerButton::Left => options.left_color,
            PointerButton::Right => options.right_color,
            _ => options.middle_color,
        };
        let radius = (7.0 + ripple.progress * 28.0) * options.pointer_scale;
        ring(
            image,
            ripple.x,
            ripple.y,
            radius,
            2.5 * options.pointer_scale,
            color,
            1.0 - ripple.progress,
        );
    }
    if let Some(wheel) = frame.wheel {
        let direction = if wheel.positive { -1.0 } else { 1.0 };
        let (dx, dy) = if wheel.horizontal {
            (direction * 24.0, 0.0)
        } else {
            (0.0, direction * 24.0)
        };
        ring(
            image,
            wheel.x + dx,
            wheel.y + dy,
            5.0,
            3.0,
            options.middle_color,
            wheel.opacity,
        );
    }
    if let Some(pointer) = frame.pointer {
        let scale = options.pointer_scale;
        triangle(
            image,
            [
                (pointer.x, pointer.y),
                (pointer.x + 4.0 * scale, pointer.y + 22.0 * scale),
                (pointer.x + 16.0 * scale, pointer.y + 15.0 * scale),
            ],
            [0, 0, 0, 255],
        );
        triangle(
            image,
            [
                (pointer.x + 2.0 * scale, pointer.y + 3.0 * scale),
                (pointer.x + 5.0 * scale, pointer.y + 18.0 * scale),
                (pointer.x + 13.0 * scale, pointer.y + 14.0 * scale),
            ],
            [255, 255, 255, 255],
        );
    }
    for (index, bubble) in frame.keys.iter().rev().enumerate() {
        let height = options.key_font_size as i32 + 16;
        let width = (bubble.text.chars().count() as i32 * options.key_font_size as i32 * 3 / 4
            + 24)
            .min(image.width as i32 - 24)
            .max(0);
        let left = 12;
        let top = image.height as i32 - 12 - (index as i32 + 1) * (height + 6);
        if top < 0 {
            break;
        }
        for y in top..top + height {
            for x in left..left + width {
                let dx = (left + 7 - x).max(x - (left + width - 8)).max(0);
                let dy = (top + 7 - y).max(y - (top + height - 8)).max(0);
                if dx * dx + dy * dy <= 49 {
                    blend(image, x, y, options.key_background_color, bubble.opacity);
                }
            }
        }
        #[cfg(windows)]
        draw_text(
            image,
            &bubble.text,
            (left + 10, top + 8),
            width.saturating_sub(20),
            options.key_font_size,
            options.key_text_color,
            bubble.opacity,
        )?;
    }
    Ok(())
}
fn blend(image: &mut CapturedImage, x: i32, y: i32, color: [u8; 4], opacity: f64) {
    if x < 0 || y < 0 || x >= image.width as i32 || y >= image.height as i32 {
        return;
    }
    let index = (y as usize * image.width as usize + x as usize) * 4;
    let alpha = f64::from(color[3]) / 255.0 * opacity.clamp(0.0, 1.0);
    for (channel, source) in [color[2], color[1], color[0]].iter().enumerate() {
        image.pixels[index + channel] = (f64::from(*source) * alpha
            + f64::from(image.pixels[index + channel]) * (1.0 - alpha))
            .round() as u8;
    }
    image.pixels[index + 3] = 255;
}
fn ring(
    image: &mut CapturedImage,
    x: f64,
    y: f64,
    radius: f64,
    thickness: f64,
    color: [u8; 4],
    opacity: f64,
) {
    let left = ((x - radius - thickness).floor() as i32).max(0);
    let right = ((x + radius + thickness).ceil() as i32).min(image.width as i32);
    let top = ((y - radius - thickness).floor() as i32).max(0);
    let bottom = ((y + radius + thickness).ceil() as i32).min(image.height as i32);
    for py in top..bottom {
        for px in left..right {
            let distance = ((f64::from(px) - x).hypot(f64::from(py) - y) - radius).abs();
            let coverage = (thickness / 2.0 + 0.5 - distance).clamp(0.0, 1.0);
            if coverage > 0.0 {
                blend(image, px, py, color, opacity * coverage);
            }
        }
    }
}
fn triangle(image: &mut CapturedImage, vertices: [(f64, f64); 3], color: [u8; 4]) {
    let min_x = vertices
        .iter()
        .map(|v| v.0 as i32)
        .min()
        .unwrap_or(0)
        .max(0);
    let max_x = vertices
        .iter()
        .map(|v| v.0.ceil() as i32)
        .max()
        .unwrap_or(0)
        .min(image.width as i32);
    let min_y = vertices
        .iter()
        .map(|v| v.1 as i32)
        .min()
        .unwrap_or(0)
        .max(0);
    let max_y = vertices
        .iter()
        .map(|v| v.1.ceil() as i32)
        .max()
        .unwrap_or(0)
        .min(image.height as i32);
    for y in min_y..max_y {
        for x in min_x..max_x {
            let mut signs = [0.0; 3];
            for i in 0..3 {
                let (a, b) = (vertices[i], vertices[(i + 1) % 3]);
                signs[i] = (f64::from(x) - a.0) * (b.1 - a.1) - (f64::from(y) - a.1) * (b.0 - a.0);
            }
            if signs.iter().all(|v| *v >= 0.0) || signs.iter().all(|v| *v <= 0.0) {
                blend(image, x, y, color, 1.0);
            }
        }
    }
}

#[cfg(windows)]
fn draw_text(
    image: &mut CapturedImage,
    text: &str,
    origin: (i32, i32),
    width: i32,
    size: u32,
    color: [u8; 4],
    opacity: f64,
) -> Result<()> {
    let (x, y) = origin;
    use windows::{
        Win32::{
            Foundation::{COLORREF, RECT},
            Graphics::Gdi::*,
        },
        core::w,
    };
    if width <= 0 {
        return Ok(());
    }
    let height = size + 6;
    unsafe {
        let dc = CreateCompatibleDC(None);
        ensure!(!dc.is_invalid(), "create key bubble DC failed");
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let bitmap = match CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(bitmap) => bitmap,
            Err(error) => {
                let _ = DeleteDC(dc);
                return Err(error.into());
            }
        };
        let old_bitmap = SelectObject(dc, bitmap.into());
        let font = CreateFontW(
            -(size as i32),
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            DEFAULT_PITCH.0 as u32,
            w!("Segoe UI"),
        );
        if font.is_invalid() {
            SelectObject(dc, old_bitmap);
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(dc);
            anyhow::bail!("create key bubble font failed");
        }
        let old_font = SelectObject(dc, font.into());
        std::ptr::write_bytes(bits, 0, width as usize * height as usize * 4);
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, COLORREF(0x00ff_ffff));
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        let mut bounds = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height as i32,
        };
        DrawTextW(
            dc,
            &mut wide,
            &mut bounds,
            DT_LEFT | DT_TOP | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
        let _ = GdiFlush();
        let bytes =
            std::slice::from_raw_parts(bits.cast::<u8>(), width as usize * height as usize * 4);
        for row in 0..height as usize {
            for column in 0..width as usize {
                let coverage = f64::from(bytes[(row * width as usize + column) * 4]) / 255.0;
                if coverage > 0.0 {
                    blend(
                        image,
                        x + column as i32,
                        y + row as i32,
                        color,
                        opacity * coverage,
                    );
                }
            }
        }
        SelectObject(dc, old_font);
        SelectObject(dc, old_bitmap);
        let _ = DeleteObject(font.into());
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(dc);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pointer(time: i64, x: f64, action: PointerAction) -> TimedEvent {
        TimedEvent {
            time_100ns: time,
            event: InputEvent::Pointer { x, y: 10.0, action },
        }
    }
    fn key(time: i64, virtual_key: u32, modifiers: u8) -> TimedEvent {
        TimedEvent {
            time_100ns: time,
            event: InputEvent::Key {
                virtual_key,
                scan_code: 0,
                down: true,
                modifiers,
            },
        }
    }
    #[test]
    fn smooth_pointer_has_no_future_event_leak_and_ripples_expire() {
        let timeline = Timeline::new(vec![
            pointer(0, 0.0, PointerAction::Move),
            pointer(1_000_000, 100.0, PointerAction::Down(PointerButton::Left)),
        ]);
        let options = EffectOptions::default();
        assert_eq!(timeline.at(-1, &options).pointer, None);
        let before = timeline.at(250_000, &options);
        assert_eq!(before.pointer.unwrap().x, 15.625);
        assert!(before.ripples.is_empty());
        assert_eq!(timeline.at(1_000_000, &options).ripples.len(), 1);
        assert!(timeline.at(7_000_000, &options).ripples.is_empty());
    }
    #[test]
    fn key_filter_merge_history_and_trim_share_absolute_timeline() {
        let timeline = Timeline::new(vec![
            key(0, 0x41, 0),
            key(1_000_000, 0x42, 0),
            key(2_000_000, 0x43, 1),
        ]);
        let mut options = EffectOptions::default();
        let frame = timeline.at(2_000_000, &options);
        assert_eq!(frame.keys[0].text, "AB");
        assert_eq!(frame.keys[1].text, "Ctrl+C");
        options.show_combinations = false;
        assert_eq!(timeline.at(2_000_000, &options).keys.len(), 1);
        options.character_merge_limit = 1;
        options.key_history_rows = 1;
        assert_eq!(timeline.at(2_000_000, &options).keys[0].text, "B");
        assert!(timeline.at(30_000_000, &options).keys.is_empty());
    }
    #[test]
    fn modifier_repeats_do_not_hide_real_keys_and_release_rearms() {
        let release = TimedEvent {
            time_100ns: 400,
            event: InputEvent::Key {
                virtual_key: 0x11,
                scan_code: 0,
                down: false,
                modifiers: 0,
            },
        };
        let timeline = Timeline::new(vec![
            key(0, 0x11, 1),
            key(100, 0x11, 1),
            key(200, 0x41, 1),
            key(300, 0x41, 1),
            release,
            key(500, 0x11, 1),
        ]);
        let frame = timeline.at(500, &EffectOptions::default());
        assert_eq!(
            frame
                .keys
                .iter()
                .map(|key| key.text.as_str())
                .collect::<Vec<_>>(),
            vec!["Ctrl", "Ctrl+A", "Ctrl+A", "Ctrl"]
        );
    }
}
