use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use crate::{CapturedImage, Point, Rect};
use anyhow::{Result, bail, ensure};

#[cfg(windows)]
use std::{cell::RefCell, thread, time::Instant};

#[cfg(windows)]
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::{
            BeginPaint, EndPaint, GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO,
            MonitorFromPoint, PAINTSTRUCT, ScreenToClient,
        },
        System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentProcessId},
        UI::{HiDpi::GetDpiForWindow, Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
    core::{BOOL, w},
};

#[cfg(windows)]
use crate::capture_overlay::{UiCanvas, ui_rect};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    Vertical,
    Horizontal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollChannel {
    Input,
    WindowMessage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StitchUpdate {
    Unchanged,
    Advanced { pixels: u32 },
    RolledBack { pixels: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LongShotAction {
    Copy,
    Pin,
    Save,
    Cancel,
}

#[derive(Clone, Debug)]
pub struct LongShotOutcome {
    /// A cancelled outcome must be discarded, even if it contains accepted frames.
    pub image: CapturedImage,
    pub action: LongShotAction,
}

/// Keeps the current viewport's position separately from the assembled image's extent.
pub struct LongShot {
    axis: Axis,
    auto_crop: bool,
    previous: CapturedImage,
    image: CapturedImage,
    position: i64,
    origin: i64,
    max_pixels: usize,
}

impl LongShot {
    pub fn new(first: CapturedImage, axis: Axis, auto_crop: bool) -> Result<Self> {
        validate(&first)?;
        Ok(Self {
            image: first.clone(),
            previous: first,
            axis,
            auto_crop,
            position: 0,
            origin: 0,
            max_pixels: 128 * 1024 * 1024,
        })
    }

    pub fn image(&self) -> &CapturedImage {
        &self.image
    }
    pub fn into_image(self) -> CapturedImage {
        self.image
    }

    pub fn push(&mut self, next: CapturedImage) -> Result<StitchUpdate> {
        validate(&next)?;
        ensure!(
            next.width == self.previous.width && next.height == self.previous.height,
            "scroll viewport dimensions changed"
        );
        let Ok(displacement) = overlap_displacement(&self.previous, &next, self.axis) else {
            // Animation or low-texture frames are inconclusive. Keep the last
            // accepted viewport so a later frame can still overlap it.
            return Ok(StitchUpdate::Unchanged);
        };
        if displacement == 0 {
            self.previous = next;
            return Ok(StitchUpdate::Unchanged);
        }
        let new_position = self.position + i64::from(displacement);
        let viewport = i64::from(extent(&next, self.axis));
        let origin = self.origin.min(new_position);
        let old_end = self.origin + i64::from(extent(&self.image, self.axis));
        let end = if displacement < 0 && self.auto_crop {
            new_position + viewport
        } else {
            old_end.max(new_position + viewport)
        };
        let length = u32::try_from(end - origin)?;
        let (width, height) = match self.axis {
            Axis::Vertical => (next.width, length),
            Axis::Horizontal => (length, next.height),
        };
        let count = (width as usize)
            .checked_mul(height as usize)
            .ok_or_else(|| anyhow::anyhow!("long screenshot size overflow"))?;
        ensure!(
            count <= self.max_pixels,
            "long screenshot exceeds pixel budget"
        );
        let mut image = CapturedImage {
            width,
            height,
            pixels: vec![0; count * 4],
        };
        blit_axis(
            &self.image,
            &mut image,
            (self.origin - origin) as u32,
            self.axis,
        );
        blit_axis(&next, &mut image, (new_position - origin) as u32, self.axis);
        self.image = image;
        self.previous = next;
        self.position = new_position;
        self.origin = origin;
        Ok(if displacement > 0 {
            StitchUpdate::Advanced {
                pixels: displacement as u32,
            }
        } else {
            StitchUpdate::RolledBack {
                pixels: displacement.unsigned_abs(),
            }
        })
    }
}

fn validate(image: &CapturedImage) -> Result<()> {
    ensure!(image.width > 0 && image.height > 0, "empty scroll viewport");
    ensure!(
        u64::from(image.width) * u64::from(image.height) * 4 == image.pixels.len() as u64,
        "invalid BGRA scroll viewport"
    );
    Ok(())
}

fn extent(image: &CapturedImage, axis: Axis) -> u32 {
    match axis {
        Axis::Vertical => image.height,
        Axis::Horizontal => image.width,
    }
}

fn blit_axis(source: &CapturedImage, destination: &mut CapturedImage, offset: u32, axis: Axis) {
    let (ox, oy) = match axis {
        Axis::Vertical => (0, offset),
        Axis::Horizontal => (offset, 0),
    };
    let width = source.width.min(destination.width.saturating_sub(ox));
    let height = source.height.min(destination.height.saturating_sub(oy));
    for y in 0..height {
        let src = (y * source.width) as usize * 4;
        let dst = ((y + oy) * destination.width + ox) as usize * 4;
        destination.pixels[dst..dst + width as usize * 4]
            .copy_from_slice(&source.pixels[src..src + width as usize * 4]);
    }
}

/// Positive displacement advances down/right. Dynamic row/column activity
/// suppresses fixed chrome; either endpoint's mask admits an overlapping pixel.
pub fn overlap_displacement(
    previous: &CapturedImage,
    next: &CapturedImage,
    axis: Axis,
) -> Result<i32> {
    validate(previous)?;
    validate(next)?;
    ensure!(
        previous.width == next.width && previous.height == next.height,
        "viewport size changed"
    );
    let length = i32::try_from(extent(previous, axis))?;
    let cross = match axis {
        Axis::Vertical => previous.width,
        Axis::Horizontal => previous.height,
    } as usize;
    let gray = |image: &CapturedImage| {
        let mut values = vec![0u8; image.pixels.len() / 4];
        for along in 0..length as usize {
            for across in 0..cross {
                let pixel = match axis {
                    Axis::Vertical => along * cross + across,
                    Axis::Horizontal => across * length as usize + along,
                } * 4;
                let channels = &image.pixels[pixel..pixel + 3];
                values[along * cross + across] = ((29 * u32::from(channels[0])
                    + 150 * u32::from(channels[1])
                    + 77 * u32::from(channels[2])
                    + 128)
                    >> 8) as u8;
            }
        }
        values
    };
    let previous = gray(previous);
    let next = gray(next);
    let dynamic: Vec<bool> = previous
        .iter()
        .zip(&next)
        .map(|(p, n)| p.abs_diff(*n) > 3)
        .collect();
    if previous
        .iter()
        .zip(&next)
        .all(|(p, n)| p.abs_diff(*n) <= 10)
    {
        return Ok(0);
    }
    let mut row_activity = vec![0.0; length as usize];
    let mut column_activity = vec![0.0; cross];
    for (index, &changed) in dynamic.iter().enumerate() {
        if changed {
            row_activity[index / cross] += 1.0 / cross as f64;
            column_activity[index % cross] += 1.0 / f64::from(length);
        }
    }
    let edges = |values: &[u8]| {
        let mut edges = vec![0u8; values.len()];
        for along in 0..length as usize {
            for across in 0..cross {
                let index = along * cross + across;
                let mut gradient = 0;
                for neighbor in [
                    along.saturating_sub(1) * cross + across,
                    (along + 1).min(length as usize - 1) * cross + across,
                    along * cross + across.saturating_sub(1),
                    along * cross + (across + 1).min(cross - 1),
                ] {
                    gradient = gradient.max(values[index].abs_diff(values[neighbor]));
                }
                edges[index] = gradient;
            }
        }
        edges
    };
    let previous_edges = edges(&previous);
    let next_edges = edges(&next);
    // Pick an edge in each sampling cell rather than a fixed grid pixel,
    // which can repeatedly land between text rows or on blank backgrounds.
    let samples = |edges: &[u8]| {
        let along_step = (length as usize / 96).max(1);
        let across_step = (cross / 96).max(1);
        let mut samples = Vec::new();
        for along in (0..length as usize).step_by(along_step) {
            for across in (0..cross).step_by(across_step) {
                let mut best = along * cross + across;
                for y in along..(along + along_step).min(length as usize) {
                    for x in across..(across + across_step).min(cross) {
                        let index = y * cross + x;
                        if edges[index] > edges[best] {
                            best = index;
                        }
                    }
                }
                samples.push(best);
            }
        }
        samples
    };
    let previous_samples = samples(&previous_edges);
    let next_samples = samples(&next_edges);
    let minimum = (length / 5).max(8).min(length);
    let score = |shift: i32| -> Option<f64> {
        let start = shift.max(0);
        let stop = length + shift.min(0);
        let mut mismatches = 0.0;
        let mut weight_sum = 0.0;
        let mut sum = 0.0;
        let mut squared = 0.0;
        let mut count = 0usize;
        for (samples, offset) in [(&previous_samples, 0), (&next_samples, shift)] {
            for &sample in samples {
                let along = (sample / cross) as i32 + offset;
                if along < start || along >= stop {
                    continue;
                }
                let shifted = (along - shift) as usize;
                let across = sample % cross;
                let p = along as usize * cross + across;
                let n = shifted * cross + across;
                if !dynamic[p] && !dynamic[n] {
                    continue;
                }
                // One pixel of support admits doubled scanlines; the edge
                // weight keeps sparse glyphs significant on flat backgrounds.
                let row_weight =
                    row_activity[along as usize].min(row_activity[shifted]) + 1.0 / cross as f64;
                let edge_weight = 1.0 + f64::from(previous_edges[p].max(next_edges[n])) / 16.0;
                let weight = row_weight * column_activity[across] * edge_weight;
                if weight == 0.0 {
                    continue;
                }
                count += 1;
                weight_sum += weight;
                let value = f64::from(previous[p]);
                sum += value * weight;
                squared += value * value * weight;
                if previous[p].abs_diff(next[n]) > 10 {
                    mismatches += weight;
                }
            }
        }
        if count < (previous.len() / 4).clamp(4, 32) || weight_sum == 0.0 {
            return None;
        }
        let mean = sum / weight_sum;
        if squared / weight_sum - mean * mean < 9.0 {
            return None;
        }
        Some(mismatches / weight_sum)
    };
    let stationary = score(0).unwrap_or(1.0);
    let mut candidates = Vec::new();
    for shift in -(length - minimum)..=length - minimum {
        if let Some(ratio) = score(shift) {
            candidates.push((ratio, shift));
        }
    }
    candidates.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.abs().cmp(&b.1.abs())));
    let Some(&(ratio, shift)) = candidates.first() else {
        bail!("scroll overlap has insufficient texture");
    };
    ensure!(
        ratio <= 0.18 && ratio < stationary * 0.85,
        "no reliable scrolling overlap; use a smaller scroll step"
    );
    if let Some(&(runner_up, _)) = candidates
        .iter()
        .find(|(_, other)| (other - shift).abs() > 6)
    {
        ensure!(
            runner_up > ratio + 0.005,
            "ambiguous repeated scroll content"
        );
    }
    Ok(shift)
}

#[derive(Clone, Debug)]
pub struct AutoScrollOptions {
    pub region: Rect,
    pub target: Point,
    pub axis: Axis,
    pub channel: ScrollChannel,
    /// Signed wheel notches: negative moves down, positive moves right.
    pub steps: i16,
    pub settle: Duration,
    pub max_frames: usize,
    pub auto_crop_on_scroll_back: bool,
}

/// Owns a native HUD on the calling worker. Pausing stops wheel injection, while
/// manual scrolling continues to be captured. Cancellation always means discard.
#[cfg(windows)]
pub fn capture_automatically(
    options: &AutoScrollOptions,
    cancel: &AtomicBool,
    mut preview: impl FnMut(&CapturedImage, StitchUpdate),
) -> Result<LongShotOutcome> {
    ensure!(
        options.max_frames > 0 && options.steps != 0,
        "invalid automatic scroll limits"
    );
    options.region.validate()?;
    let mut shortcuts = ShortcutLatch::new(HudKeys::read());
    let first = crate::capture_overlay::capture_rect(options.region)?;
    if cancel.load(Ordering::Acquire) {
        return Ok(LongShotOutcome {
            image: first,
            action: LongShotAction::Cancel,
        });
    }
    let target = ScrollTarget::resolve(options.target)?;
    let mut shot = LongShot::new(first, options.axis, options.auto_crop_on_scroll_back)?;
    let hud = LongShotHud::new(options.region, shot.image())?;
    if options.channel == ScrollChannel::Input {
        unsafe {
            let _ = SetForegroundWindow(target.root);
        }
    }
    let started = Instant::now();
    let interval = options.settle.max(Duration::from_millis(100));
    let mut capture_at = Instant::now() + interval;
    let mut wheel_pending = false;
    let mut was_paused = false;
    loop {
        hud.pump(&mut shortcuts, started.elapsed(), cancel)?;
        if let Some(action) = terminal_action(cancel, hud.state.borrow().controls.action) {
            return Ok(LongShotOutcome {
                image: shot.into_image(),
                action,
            });
        }
        let paused = hud.state.borrow().controls.paused;
        if was_paused && !paused {
            wheel_pending = false;
        }
        was_paused = paused;
        if !paused && !wheel_pending {
            if shortcuts.previous.blocks_scroll() {
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            scroll_window(
                options.target,
                target,
                options.axis,
                options.steps,
                options.channel,
            )?;
            wheel_pending = true;
            capture_at = Instant::now() + interval;
        }
        if Instant::now() < capture_at {
            thread::sleep(Duration::from_millis(10));
            continue;
        }
        let automatic = wheel_pending;
        // Capture and matching can be expensive. The HUD's owning thread keeps
        // dispatching messages until the frame worker has released the stitcher.
        let update = thread::scope(|scope| -> Result<StitchUpdate> {
            let work = scope.spawn(|| {
                let frame = crate::capture_overlay::capture_rect(options.region)?;
                if cancel.load(Ordering::Acquire) {
                    return Ok(StitchUpdate::Unchanged);
                }
                shot.push(frame)
            });
            while !work.is_finished() {
                hud.pump(&mut shortcuts, started.elapsed(), cancel)?;
                thread::sleep(Duration::from_millis(10));
            }
            work.join()
                .map_err(|_| anyhow::anyhow!("long screenshot frame worker panicked"))?
        });
        if let Some(action) = terminal_action(cancel, hud.state.borrow().controls.action) {
            return Ok(LongShotOutcome {
                image: shot.into_image(),
                action,
            });
        }
        let update = update?;
        preview(shot.image(), update);
        {
            let mut state = hud.state.borrow_mut();
            state
                .controls
                .captured(shot.image(), update, automatic, options.max_frames);
            state.dirty = true;
        }
        if update != StitchUpdate::Unchanged {
            let frames = hud.state.borrow().controls.frames;
            hud.preview.draw(shot.image(), frames)?;
        }
        wheel_pending = false;
        capture_at = Instant::now() + interval;
    }
}

#[cfg(windows)]
pub fn scroll(target: Point, axis: Axis, steps: i16, channel: ScrollChannel) -> Result<()> {
    let window = ScrollTarget::resolve(target)?;
    if channel == ScrollChannel::Input {
        unsafe {
            let _ = SetForegroundWindow(window.root);
        }
    }
    scroll_window(target, window, axis, steps, channel)
}

#[cfg(test)]
fn capture_frames(
    options: &AutoScrollOptions,
    cancel: &AtomicBool,
    first: CapturedImage,
    mut preview: impl FnMut(&CapturedImage, StitchUpdate),
    mut next_frame: impl FnMut() -> Result<Option<CapturedImage>>,
) -> Result<CapturedImage> {
    let mut shot = LongShot::new(first, options.axis, options.auto_crop_on_scroll_back)?;
    let mut unchanged = 0;
    for _ in 0..options.max_frames {
        if cancel.load(Ordering::Acquire) {
            break;
        }
        let Some(frame) = next_frame()? else {
            break;
        };
        let update = shot.push(frame)?;
        preview(shot.image(), update);
        unchanged = if update == StitchUpdate::Unchanged {
            unchanged + 1
        } else {
            0
        };
        if unchanged == 2 {
            break;
        }
    }
    Ok(shot.into_image())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HudCommand {
    TogglePause,
    Finish(LongShotAction),
}

#[derive(Clone, Copy, Default)]
struct HudKeys {
    escape: bool,
    left_button: bool,
    right_button: bool,
    enter: bool,
    f2: bool,
    space: bool,
    s: bool,
    control: bool,
    shift: bool,
    alt: bool,
    meta: bool,
}

impl HudKeys {
    fn blocks_scroll(self) -> bool {
        self.control
            || self.shift
            || self.alt
            || self.meta
            || self.space
            || self.enter
            || self.escape
            || self.f2
            || self.s
            || self.left_button
            || self.right_button
    }

    #[cfg(windows)]
    fn read() -> Self {
        let down = |key: VIRTUAL_KEY| unsafe { GetAsyncKeyState(i32::from(key.0)) < 0 };
        Self {
            escape: down(VK_ESCAPE),
            left_button: down(VK_LBUTTON),
            right_button: down(VK_RBUTTON),
            enter: down(VK_RETURN),
            f2: down(VK_F2),
            space: down(VK_SPACE),
            s: down(VK_S),
            control: down(VK_CONTROL),
            shift: down(VK_SHIFT),
            alt: down(VK_MENU),
            meta: down(VK_LWIN) || down(VK_RWIN),
        }
    }
}

struct ShortcutLatch {
    previous: HudKeys,
}

impl ShortcutLatch {
    fn new(previous: HudKeys) -> Self {
        Self { previous }
    }

    fn poll(&mut self, elapsed: Duration, keys: HudKeys) -> Option<HudCommand> {
        let previous = std::mem::replace(&mut self.previous, keys);
        if elapsed < Duration::from_millis(200) {
            return None;
        }
        if keys.escape && !previous.escape {
            return Some(HudCommand::Finish(LongShotAction::Cancel));
        }
        if keys.right_button && !previous.right_button {
            return Some(HudCommand::Finish(LongShotAction::Copy));
        }
        if keys.shift || keys.alt || keys.meta {
            return None;
        }
        if keys.control {
            return (keys.s && !previous.s).then_some(HudCommand::Finish(LongShotAction::Save));
        }
        if keys.f2 && !previous.f2 {
            Some(HudCommand::Finish(LongShotAction::Pin))
        } else if keys.enter && !previous.enter {
            Some(HudCommand::Finish(LongShotAction::Copy))
        } else if keys.space && !previous.space {
            Some(HudCommand::TogglePause)
        } else {
            None
        }
    }
}

fn terminal_action(
    cancel: &AtomicBool,
    requested: Option<LongShotAction>,
) -> Option<LongShotAction> {
    if requested == Some(LongShotAction::Cancel) {
        cancel.store(true, Ordering::Release);
    }
    if cancel.load(Ordering::Acquire) {
        Some(LongShotAction::Cancel)
    } else {
        requested
    }
}

struct CaptureControls {
    width: u32,
    height: u32,
    frames: usize,
    paused: bool,
    action: Option<LongShotAction>,
    automatic_frames: usize,
    unchanged: usize,
}

impl CaptureControls {
    fn new(image: &CapturedImage) -> Self {
        Self {
            width: image.width,
            height: image.height,
            frames: 1,
            paused: false,
            action: None,
            automatic_frames: 0,
            unchanged: 0,
        }
    }

    fn apply(&mut self, command: HudCommand) {
        if self.action.is_some() {
            return;
        }
        match command {
            HudCommand::TogglePause => {
                self.paused = !self.paused;
                self.automatic_frames = 0;
                self.unchanged = 0;
            }
            HudCommand::Finish(action) => self.action = Some(action),
        }
    }

    fn captured(
        &mut self,
        image: &CapturedImage,
        update: StitchUpdate,
        automatic: bool,
        limit: usize,
    ) {
        self.width = image.width;
        self.height = image.height;
        if update != StitchUpdate::Unchanged {
            self.frames += 1;
        }
        if automatic {
            self.automatic_frames += 1;
            self.unchanged = if update == StitchUpdate::Unchanged {
                self.unchanged + 1
            } else {
                0
            };
            if self.automatic_frames >= limit || self.unchanged >= 2 {
                self.paused = true;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HudControl {
    Pause,
    Copy,
    Pin,
    Save,
    Cancel,
}

impl HudControl {
    fn command(self) -> HudCommand {
        match self {
            Self::Pause => HudCommand::TogglePause,
            Self::Copy => HudCommand::Finish(LongShotAction::Copy),
            Self::Pin => HudCommand::Finish(LongShotAction::Pin),
            Self::Save => HudCommand::Finish(LongShotAction::Save),
            Self::Cancel => HudCommand::Finish(LongShotAction::Cancel),
        }
    }

    fn bounds(self, scale: f64) -> Rect {
        let (x, width) = match self {
            Self::Pause => (8., 108.),
            Self::Copy => (122., 192.),
            Self::Pin => (320., 84.),
            Self::Save => (410., 120.),
            Self::Cancel => (536., 96.),
        };
        Rect {
            x: x * scale,
            y: 62. * scale,
            width: width * scale,
            height: 36. * scale,
        }
    }
}

const HUD_CONTROLS: [HudControl; 5] = [
    HudControl::Pause,
    HudControl::Copy,
    HudControl::Pin,
    HudControl::Save,
    HudControl::Cancel,
];

fn hud_hit(scale: f64, point: Point) -> Option<HudControl> {
    HUD_CONTROLS.into_iter().find(|control| {
        let bounds = control.bounds(scale);
        point.x >= bounds.x
            && point.x < bounds.x + bounds.width
            && point.y >= bounds.y
            && point.y < bounds.y + bounds.height
    })
}

#[cfg(windows)]
#[derive(Clone, Copy)]
struct ScrollTarget {
    hwnd: HWND,
    root: HWND,
    process: u32,
}

#[cfg(windows)]
impl ScrollTarget {
    fn resolve(point: Point) -> Result<Self> {
        ensure!(
            point.x.is_finite() && point.y.is_finite(),
            "invalid scroll target"
        );
        unsafe {
            let point = POINT {
                x: point.x.round() as i32,
                y: point.y.round() as i32,
            };
            let mut hwnd = WindowFromPoint(point);
            let mut process = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut process));
            if !external_process(process, GetCurrentProcessId()) {
                struct Search {
                    point: POINT,
                    hwnd: HWND,
                }
                unsafe extern "system" fn find(hwnd: HWND, data: LPARAM) -> BOOL {
                    unsafe {
                        let search = &mut *(data.0 as *mut Search);
                        let mut process = 0;
                        GetWindowThreadProcessId(hwnd, Some(&mut process));
                        let mut rect = RECT::default();
                        if external_process(process, GetCurrentProcessId())
                            && IsWindowVisible(hwnd).as_bool()
                            && !IsIconic(hwnd).as_bool()
                            && GetWindowRect(hwnd, &mut rect).is_ok()
                            && search.point.x >= rect.left
                            && search.point.x < rect.right
                            && search.point.y >= rect.top
                            && search.point.y < rect.bottom
                            && GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TRANSPARENT.0
                                == 0
                        {
                            search.hwnd = hwnd;
                            return BOOL(0);
                        }
                        BOOL(1)
                    }
                }
                let mut search = Search {
                    point,
                    hwnd: HWND::default(),
                };
                let result = EnumWindows(Some(find), LPARAM(&mut search as *mut _ as isize));
                if search.hwnd.is_invalid() {
                    result?;
                }
                hwnd = search.hwnd;
            }
            ensure!(!hwnd.is_invalid(), "No external scroll window at target");
            loop {
                let mut client = point;
                ensure!(
                    ScreenToClient(hwnd, &mut client).as_bool(),
                    "Cannot locate scroll target"
                );
                let child = ChildWindowFromPointEx(
                    hwnd,
                    client,
                    CWP_SKIPINVISIBLE | CWP_SKIPDISABLED | CWP_SKIPTRANSPARENT,
                );
                if child.is_invalid() || child == hwnd {
                    break;
                }
                hwnd = child;
            }
            GetWindowThreadProcessId(hwnd, Some(&mut process));
            ensure!(
                external_process(process, GetCurrentProcessId()),
                "Scroll target belongs to Orbment"
            );
            Ok(Self {
                hwnd,
                root: GetAncestor(hwnd, GA_ROOT),
                process,
            })
        }
    }

    fn validate(self) -> Result<()> {
        unsafe {
            let mut process = 0;
            GetWindowThreadProcessId(self.hwnd, Some(&mut process));
            ensure!(
                IsWindow(Some(self.hwnd)).as_bool()
                    && process == self.process
                    && external_process(process, GetCurrentProcessId()),
                "The locked scroll target no longer exists"
            );
        }
        Ok(())
    }
}

fn external_process(process: u32, current: u32) -> bool {
    process != 0 && process != current
}

#[cfg(windows)]
fn scroll_window(
    target: Point,
    window: ScrollTarget,
    axis: Axis,
    steps: i16,
    channel: ScrollChannel,
) -> Result<()> {
    window.validate()?;
    ensure!(
        target.x.is_finite() && target.y.is_finite(),
        "invalid scroll target"
    );
    let x = target.x.round() as i32;
    let y = target.y.round() as i32;
    let delta = i32::from(steps)
        .checked_mul(120)
        .and_then(|v| i16::try_from(v).ok())
        .ok_or_else(|| anyhow::anyhow!("wheel delta exceeds signed 16-bit range"))?;
    unsafe {
        let mut cursor = POINT::default();
        if channel == ScrollChannel::Input
            && GetCursorPos(&mut cursor).is_ok()
            && GetAncestor(WindowFromPoint(cursor), GA_ROOT) == window.root
            && GetAncestor(GetForegroundWindow(), GA_ROOT) == window.root
        {
            let input = INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        mouseData: delta as i32 as u32,
                        dwFlags: match axis {
                            Axis::Vertical => MOUSEEVENTF_WHEEL,
                            Axis::Horizontal => MOUSEEVENTF_HWHEEL,
                        },
                        ..Default::default()
                    },
                },
            };
            if SendInput(&[input], std::mem::size_of::<INPUT>() as i32) == 1 {
                return Ok(());
            }
        }
        ensure!(
            i16::try_from(x).is_ok() && i16::try_from(y).is_ok(),
            "wheel screen coordinates exceed Win32 signed 16-bit range"
        );
        let packed = u32::from(x as i16 as u16) | (u32::from(y as i16 as u16) << 16);
        let message = match axis {
            Axis::Vertical => WM_MOUSEWHEEL,
            Axis::Horizontal => WM_MOUSEHWHEEL,
        };
        let wparam = WPARAM((delta as u16 as usize) << 16);
        let lparam = LPARAM(packed as i32 as isize);
        // DefWindowProc propagates wheel messages to the parent. Only retry
        // the root if posting to the child fails, avoiding doubled scrolling.
        PostMessageW(Some(window.hwnd), message, wparam, lparam).or_else(|error| {
            if window.hwnd == window.root {
                Err(error)
            } else {
                PostMessageW(Some(window.root), message, wparam, lparam)
            }
        })?;
    }
    Ok(())
}

fn preview_bounds(region: Rect, work: Rect, scale: f64) -> Rect {
    let gap = 12. * scale;
    let right = work.x + work.width;
    let right_space = right - (region.x + region.width + gap);
    let left_space = region.x - gap - work.x;
    let minimum = 180. * scale;
    let (width, x) = if right_space >= minimum {
        (
            (200. * scale).min(right_space),
            region.x + region.width + gap,
        )
    } else if left_space >= minimum {
        let width = (200. * scale).min(left_space);
        (width, region.x - width - gap)
    } else {
        let width = (200. * scale).min(work.width);
        (width, right - width)
    };
    let height = region
        .height
        .clamp(240. * scale, (work.height - 140. * scale).max(240. * scale))
        .min(work.height);
    Rect {
        x: x.clamp(work.x, (right - width).max(work.x)),
        y: region
            .y
            .clamp(work.y, (work.y + work.height - height).max(work.y)),
        width,
        height,
    }
}

fn hud_bounds(region: Rect, work: Rect, scale: f64) -> Rect {
    let width = (640. * scale).ceil();
    let height = (112. * scale).ceil();
    let gap = 8. * scale;
    let below = region.y + region.height + gap;
    let y = if below + height <= work.y + work.height {
        below
    } else {
        region.y - height - gap
    };
    Rect {
        x: (region.x + region.width - width)
            .clamp(work.x, (work.x + work.width - width).max(work.x)),
        y: y.clamp(work.y, (work.y + work.height - height).max(work.y)),
        width,
        height,
    }
}

#[cfg(windows)]
fn backdrop_canvas(region: Rect, monitor: Rect, scale: f64) -> Result<UiCanvas> {
    let mut canvas = UiCanvas::new(monitor.width as u32, monitor.height as u32)?;
    let region = ui_rect(
        region.x - monitor.x,
        region.y - monitor.y,
        region.width,
        region.height,
    );
    let left = region.x.clamp(0., monitor.width);
    let right = (region.x + region.width).clamp(0., monitor.width);
    let top = region.y.clamp(0., monitor.height);
    let bottom = (region.y + region.height).clamp(0., monitor.height);
    for band in [
        ui_rect(0., 0., monitor.width, top),
        ui_rect(0., bottom, monitor.width, monitor.height - bottom),
        ui_rect(0., top, left, bottom - top),
        ui_rect(right, top, monitor.width - right, bottom - top),
    ] {
        canvas.rounded(band, 0., 0x80000000, None);
    }
    let border = 2.5 * scale;
    canvas.rounded(
        ui_rect(
            region.x - border / 2.,
            region.y - border / 2.,
            region.width + border,
            region.height + border,
        ),
        0.,
        0xff1677ff,
        Some(border as f32),
    );
    Ok(canvas)
}

#[cfg(windows)]
struct LongShotWindow(HWND);

#[cfg(windows)]
impl LongShotWindow {
    fn new(bounds: Rect, interactive: bool) -> Result<Self> {
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let class = if interactive {
                w!("OrbmentLongShotHud")
            } else {
                w!("OrbmentLongShotLayer")
            };
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(if interactive {
                    long_shot_hud_proc
                } else {
                    long_shot_layer_proc
                }),
                hInstance: instance.into(),
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                lpszClassName: class,
                ..Default::default()
            });
            let mut style = WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED;
            if !interactive {
                style |= WS_EX_TRANSPARENT;
            }
            let window = Self(CreateWindowExW(
                style,
                class,
                w!("长截图"),
                WS_POPUP,
                bounds.x.round() as i32,
                bounds.y.round() as i32,
                bounds.width.ceil() as i32,
                bounds.height.ceil() as i32,
                None,
                None,
                Some(instance.into()),
                None,
            )?);
            SetWindowDisplayAffinity(window.0, WDA_EXCLUDEFROMCAPTURE)?;
            Ok(window)
        }
    }

    fn present(&self, canvas: UiCanvas) -> Result<()> {
        canvas.present_layered(self.0)?;
        unsafe {
            let _ = ShowWindow(self.0, SW_SHOWNOACTIVATE);
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for LongShotWindow {
    fn drop(&mut self) {
        unsafe {
            SetWindowLongPtrW(self.0, GWLP_USERDATA, 0);
            let _ = DestroyWindow(self.0);
        }
    }
}

#[cfg(windows)]
unsafe extern "system" fn long_shot_layer_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

#[cfg(windows)]
struct LongShotPreview {
    window: LongShotWindow,
    bounds: Rect,
    scale: f64,
}

#[cfg(windows)]
impl LongShotPreview {
    fn new(region: Rect, work: Rect, scale: f64, image: &CapturedImage) -> Result<Self> {
        let bounds = preview_bounds(region, work, scale);
        let preview = Self {
            window: LongShotWindow::new(bounds, false)?,
            bounds,
            scale,
        };
        preview.draw(image, 1)?;
        Ok(preview)
    }

    fn draw(&self, image: &CapturedImage, frames: usize) -> Result<()> {
        use crate::canvas_mark::render_ui_text;

        let scale = self.scale;
        let width = self.bounds.width.ceil() as u32;
        let height = self.bounds.height.ceil() as u32;
        let mut canvas = UiCanvas::new(width, height)?;
        canvas.panel(ui_rect(
            3. * scale,
            3. * scale,
            f64::from(width) - 6. * scale,
            f64::from(height) - 6. * scale,
        ));
        let label = format!("{} × {} px · {} 帧", image.width, image.height, frames);
        let title_bounds = ui_rect(
            10. * scale,
            7. * scale,
            self.bounds.width - 20. * scale,
            26. * scale,
        );
        let mut title = render_ui_text(&label, 12. * scale, true, 0xff1e293b)?;
        if f64::from(title.width()) > title_bounds.width {
            title = render_ui_text(
                &label,
                12. * scale * title_bounds.width / f64::from(title.width()),
                true,
                0xff1e293b,
            )?;
        }
        canvas.centered(&title, title_bounds);
        let content = ui_rect(
            10. * scale,
            40. * scale,
            self.bounds.width - 20. * scale,
            self.bounds.height - 50. * scale,
        );
        canvas.rounded(content, 4. * scale, 0xffe2e8f0, None);
        let ratio =
            (content.width / f64::from(image.width)).min(content.height / f64::from(image.height));
        let width = (f64::from(image.width) * ratio).floor().max(1.) as u32;
        let height = (f64::from(image.height) * ratio).floor().max(1.) as u32;
        let source = image::ImageBuffer::<image::Rgba<u8>, _>::from_raw(
            image.width,
            image.height,
            image.pixels.as_slice(),
        )
        .ok_or_else(|| anyhow::anyhow!("invalid long screenshot preview pixels"))?;
        let thumbnail = image::imageops::thumbnail(&source, width, height);
        let mut pixmap = tiny_skia::Pixmap::new(width, height)
            .ok_or_else(|| anyhow::anyhow!("cannot allocate long screenshot preview"))?;
        for (destination, source) in pixmap
            .data_mut()
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(thumbnail.as_raw().as_chunks::<4>().0)
        {
            let alpha = u16::from(source[3]);
            *destination = [
                (u16::from(source[2]) * alpha / 255) as u8,
                (u16::from(source[1]) * alpha / 255) as u8,
                (u16::from(source[0]) * alpha / 255) as u8,
                source[3],
            ];
        }
        canvas.centered(&pixmap, content);
        self.window.present(canvas)
    }
}

#[cfg(windows)]
struct HudState {
    controls: CaptureControls,
    scale: f64,
    dirty: bool,
    pressed: Option<HudControl>,
    right_pressed: bool,
    started: Instant,
}

#[cfg(windows)]
struct LongShotHud {
    window: LongShotWindow,
    state: Box<RefCell<HudState>>,
    preview: LongShotPreview,
    _backdrop: LongShotWindow,
}

#[cfg(windows)]
impl LongShotHud {
    fn new(region: Rect, image: &CapturedImage) -> Result<Self> {
        unsafe {
            let mut monitor = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            ensure!(
                GetMonitorInfoW(
                    MonitorFromPoint(
                        POINT {
                            x: (region.x + region.width / 2.) as i32,
                            y: (region.y + region.height / 2.) as i32
                        },
                        MONITOR_DEFAULTTONEAREST
                    ),
                    &mut monitor
                )
                .as_bool(),
                "Scroll HUD monitor unavailable"
            );
            let to_rect = |rect: RECT| {
                ui_rect(
                    f64::from(rect.left),
                    f64::from(rect.top),
                    f64::from(rect.right - rect.left),
                    f64::from(rect.bottom - rect.top),
                )
            };
            let monitor_bounds = to_rect(monitor.rcMonitor);
            let work = to_rect(monitor.rcWork);
            let backdrop = LongShotWindow::new(monitor_bounds, false)?;
            let display_scale = (f64::from(GetDpiForWindow(backdrop.0)) / 96.).max(1.);
            backdrop.present(backdrop_canvas(region, monitor_bounds, display_scale)?)?;
            let preview = LongShotPreview::new(region, work, display_scale, image)?;
            let scale = display_scale.min(work.width / 640.).min(work.height / 112.);
            let window = LongShotWindow::new(hud_bounds(region, work, scale), true)?;
            let hwnd = window.0;
            let hud = Self {
                window,
                state: Box::new(RefCell::new(HudState {
                    controls: CaptureControls::new(image),
                    scale,
                    dirty: true,
                    pressed: None,
                    right_pressed: false,
                    started: Instant::now(),
                })),
                preview,
                _backdrop: backdrop,
            };
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, &*hud.state as *const _ as isize);
            hud.draw()?;
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            Ok(hud)
        }
    }

    fn draw(&self) -> Result<()> {
        use crate::{
            canvas_mark::render_ui_text,
            capture_overlay::{UiCanvas, ui_rect},
        };
        use orbment_core::i18n::tr;
        let canvas = {
            let mut state = self.state.borrow_mut();
            if !state.dirty {
                return Ok(());
            }
            let scale = state.scale;
            let width = (640. * scale).ceil() as u32;
            let height = (112. * scale).ceil() as u32;
            let mut canvas = UiCanvas::new(width, height)?;
            canvas.panel(ui_rect(0., 0., f64::from(width), f64::from(height)));
            let label = tr("vision.longshot_status")
                .replace("{width}", &state.controls.width.to_string())
                .replace("{height}", &state.controls.height.to_string())
                .replace("{frames}", &state.controls.frames.to_string());
            let text = render_ui_text(&label, 14. * scale, true, 0xff1e293b)?;
            canvas.centered(
                &text,
                ui_rect(8. * scale, 5. * scale, 624. * scale, 28. * scale),
            );
            let hint = tr(if state.controls.paused {
                "vision.longshot_manual"
            } else {
                "vision.longshot_auto"
            });
            let text = render_ui_text(&hint, 12. * scale, false, 0xff64748b)?;
            canvas.centered(
                &text,
                ui_rect(8. * scale, 34. * scale, 624. * scale, 22. * scale),
            );
            for control in HUD_CONTROLS {
                let bounds = control.bounds(scale);
                let key = match control {
                    HudControl::Pause if state.controls.paused => "vision.longshot_resume",
                    HudControl::Pause => "vision.longshot_pause",
                    HudControl::Copy => "vision.longshot_copy",
                    HudControl::Pin => "vision.longshot_pin",
                    HudControl::Save => "vision.longshot_save",
                    HudControl::Cancel => "vision.longshot_cancel",
                };
                canvas.rounded(
                    bounds,
                    6. * scale,
                    if state.pressed == Some(control) {
                        0xffbfdbfe
                    } else {
                        0xffe2e8f0
                    },
                    None,
                );
                let text = render_ui_text(&tr(key), 12. * scale, true, 0xff1e293b)?;
                canvas.centered(&text, bounds);
            }
            state.dirty = false;
            canvas
        };
        self.window.present(canvas)
    }

    fn pump(
        &self,
        shortcuts: &mut ShortcutLatch,
        elapsed: Duration,
        cancel: &AtomicBool,
    ) -> Result<()> {
        unsafe {
            let mut message = MSG::default();
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                if message.message == WM_QUIT {
                    cancel.store(true, Ordering::Release);
                } else {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
        }
        {
            let mut state = self.state.borrow_mut();
            if let Some(command) = shortcuts.poll(elapsed, HudKeys::read()) {
                state.controls.apply(command);
                state.dirty = true;
            }
            if let Some(action) = terminal_action(cancel, state.controls.action) {
                state.controls.action = Some(action);
            }
        }
        self.draw()
    }
}

#[cfg(windows)]
unsafe extern "system" fn long_shot_hud_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<HudState>;
        let Some(cell) = pointer.as_ref() else {
            return DefWindowProcW(hwnd, message, wparam, lparam);
        };
        match message {
            WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
            WM_ERASEBKGND => return LRESULT(1),
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                BeginPaint(hwnd, &mut paint);
                let _ = EndPaint(hwnd, &paint);
                return LRESULT(0);
            }
            _ => {}
        }
        let Ok(mut state) = cell.try_borrow_mut() else {
            return DefWindowProcW(hwnd, message, wparam, lparam);
        };
        let point = Point {
            x: f64::from(lparam.0 as u16 as i16),
            y: f64::from((lparam.0 >> 16) as u16 as i16),
        };
        match message {
            WM_LBUTTONDOWN => {
                state.pressed = hud_hit(state.scale, point);
                state.dirty = true;
                drop(state);
                SetCapture(hwnd);
                return LRESULT(0);
            }
            WM_LBUTTONUP => {
                let pressed = state.pressed.take();
                if let Some(control) =
                    pressed.filter(|control| Some(*control) == hud_hit(state.scale, point))
                {
                    state.controls.apply(control.command());
                }
                state.dirty = true;
                drop(state);
                let _ = ReleaseCapture();
                return LRESULT(0);
            }
            WM_CAPTURECHANGED => {
                state.pressed = None;
                state.right_pressed = false;
                state.dirty = true;
                return LRESULT(0);
            }
            WM_RBUTTONDOWN => {
                state.right_pressed = state.started.elapsed() >= Duration::from_millis(200);
                return LRESULT(0);
            }
            WM_RBUTTONUP => {
                if std::mem::take(&mut state.right_pressed) {
                    state
                        .controls
                        .apply(HudCommand::Finish(LongShotAction::Copy));
                }
                return LRESULT(0);
            }
            WM_CLOSE => {
                state
                    .controls
                    .apply(HudCommand::Finish(LongShotAction::Cancel));
                return LRESULT(0);
            }
            _ => {}
        }
        drop(state);
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn viewport(offset: u32, axis: Axis) -> CapturedImage {
        let (width, height) = (64, 64);
        let mut pixels = vec![0; width as usize * height as usize * 4];
        for y in 0..height {
            for x in 0..width {
                let (a, b) = match axis {
                    Axis::Vertical => (y + offset, x),
                    Axis::Horizontal => (x + offset, y),
                };
                let index = ((y * width + x) * 4) as usize;
                pixels[index..index + 4].copy_from_slice(&[
                    ((a * 73 + b * 17 + a * a * 3) % 251) as u8,
                    ((a * 13 + b * 41) % 253) as u8,
                    ((a * 43 + b * 7) % 249) as u8,
                    255,
                ]);
            }
        }
        CapturedImage {
            width,
            height,
            pixels,
        }
    }
    #[test]
    fn overlap_and_rollback_crop_both_axes() {
        for axis in [Axis::Vertical, Axis::Horizontal] {
            let mut shot = LongShot::new(viewport(0, axis), axis, true).unwrap();
            assert_eq!(
                shot.push(viewport(20, axis)).unwrap(),
                StitchUpdate::Advanced { pixels: 20 }
            );
            assert_eq!(extent(shot.image(), axis), 84);
            assert_eq!(
                shot.push(viewport(7, axis)).unwrap(),
                StitchUpdate::RolledBack { pixels: 13 }
            );
            assert_eq!(extent(shot.image(), axis), 71);
            assert_eq!(
                shot.push(viewport(7, axis)).unwrap(),
                StitchUpdate::Unchanged
            );
        }
    }
    #[test]
    fn rollback_can_preserve_previous_extent() {
        let mut shot = LongShot::new(viewport(0, Axis::Vertical), Axis::Vertical, false).unwrap();
        shot.push(viewport(20, Axis::Vertical)).unwrap();
        shot.push(viewport(4, Axis::Vertical)).unwrap();
        assert_eq!(shot.image().height, 84);
    }

    fn noisy(image: &mut CapturedImage, amount: i16) {
        for (index, pixel) in image.pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let noise = if index % 2 == 0 { amount } else { -amount };
            for value in &mut pixel[..3] {
                *value = (i16::from(*value) + noise).clamp(0, 255) as u8;
            }
        }
    }

    fn chrome_viewport(offset: u32, axis: Axis, noise: i16) -> CapturedImage {
        let mut image = viewport(offset, axis);
        for y in 0..image.height {
            for x in 0..image.width {
                let (along, across) = match axis {
                    Axis::Vertical => (y, x),
                    Axis::Horizontal => (x, y),
                };
                if !(14..56).contains(&along) || !(16..48).contains(&across) {
                    let value = (40 + (along * 13 + across * 31) % 170) as u8;
                    let index = ((y * image.width + x) * 4) as usize;
                    image.pixels[index..index + 4].copy_from_slice(&[value, value, value, 255]);
                }
            }
        }
        noisy(&mut image, noise);
        image
    }

    #[test]
    fn fixed_chrome_and_capture_noise_preserve_signed_displacement_both_axes() -> Result<()> {
        for axis in [Axis::Vertical, Axis::Horizontal] {
            let first = chrome_viewport(0, axis, 0);
            let forward = chrome_viewport(20, axis, 2);
            let backward = chrome_viewport(7, axis, 0);
            assert_eq!(overlap_displacement(&first, &forward, axis)?, 20);
            assert_eq!(overlap_displacement(&forward, &backward, axis)?, -13);
            let mut shot = LongShot::new(first, axis, true)?;
            shot.push(forward)?;
            assert_eq!(extent(shot.image(), axis), 84);
            shot.push(backward.clone())?;
            assert_eq!(extent(shot.image(), axis), 71);
            assert_eq!(shot.push(backward)?, StitchUpdate::Unchanged);
        }
        Ok(())
    }

    #[test]
    fn grayscale_tolerance_includes_ten_but_rejects_eleven_and_flat_changes() -> Result<()> {
        for axis in [Axis::Vertical, Axis::Horizontal] {
            let first = viewport(0, axis);
            let mut duplicate = first.clone();
            noisy(&mut duplicate, 10);
            assert_eq!(overlap_displacement(&first, &duplicate, axis)?, 0);
            noisy(&mut duplicate, 1);
            assert!(overlap_displacement(&first, &duplicate, axis).is_err());
            let flat = CapturedImage {
                width: 64,
                height: 64,
                pixels: vec![80; 64 * 64 * 4],
            };
            let changed = CapturedImage {
                pixels: vec![110; 64 * 64 * 4],
                ..flat.clone()
            };
            assert_eq!(overlap_displacement(&flat, &flat, axis)?, 0);
            assert!(overlap_displacement(&flat, &changed, axis).is_err());
        }
        Ok(())
    }

    #[test]
    fn mismatch_ratio_tolerates_sparse_large_errors_both_axes() -> Result<()> {
        for axis in [Axis::Vertical, Axis::Horizontal] {
            let first = viewport(0, axis);
            let mut next = viewport(18, axis);
            for (index, pixel) in next.pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                if index % 13 == 0 {
                    for value in &mut pixel[..3] {
                        *value = 255 - *value;
                    }
                }
            }
            assert_eq!(overlap_displacement(&first, &next, axis)?, 18);
        }
        Ok(())
    }

    #[test]
    fn dynamic_mask_accepts_either_endpoint_for_doubled_scanlines() -> Result<()> {
        for axis in [Axis::Vertical, Axis::Horizontal] {
            let paired = |offset: u32| {
                let mut image = viewport(0, axis);
                for y in 0..64 {
                    for x in 0..64 {
                        let (along, across) = match axis {
                            Axis::Vertical => ((y + offset) / 2, x),
                            Axis::Horizontal => ((x + offset) / 2, y),
                        };
                        let value = ((along * 73 + across * 17 + along * along * 3) % 251) as u8;
                        let index = ((y * 64 + x) * 4) as usize;
                        image.pixels[index..index + 4].copy_from_slice(&[value, value, value, 255]);
                    }
                }
                image
            };
            assert_eq!(overlap_displacement(&paired(0), &paired(1), axis)?, 1);
            assert_eq!(overlap_displacement(&paired(1), &paired(0), axis)?, -1);
        }
        Ok(())
    }

    #[test]
    fn unmatched_frames_are_skipped_without_mutating_the_reference_both_axes() -> Result<()> {
        for axis in [Axis::Vertical, Axis::Horizontal] {
            let first = viewport(0, axis);
            let mut shot = LongShot::new(first.clone(), axis, false)?;
            assert_eq!(shot.push(viewport(80, axis))?, StitchUpdate::Unchanged);
            assert_eq!(shot.image().pixels, first.pixels);
            assert_eq!(
                shot.push(viewport(16, axis))?,
                StitchUpdate::Advanced { pixels: 16 }
            );
            let accepted = shot.previous.clone();
            let assembled = shot.image().clone();
            let flat = CapturedImage {
                pixels: vec![255; 64 * 64 * 4],
                ..first.clone()
            };
            assert_eq!(shot.push(flat)?, StitchUpdate::Unchanged);
            assert_eq!(shot.previous.pixels, accepted.pixels);
            assert_eq!(shot.image().pixels, assembled.pixels);
            assert_eq!(
                shot.push(viewport(32, axis))?,
                StitchUpdate::Advanced { pixels: 16 }
            );
            let repeating = |offset: u32| {
                let mut image = viewport(0, axis);
                for y in 0..image.height {
                    for x in 0..image.width {
                        let along = if axis == Axis::Vertical { y } else { x };
                        let value = (((along + offset) % 8) * 31) as u8;
                        let index = ((y * image.width + x) * 4) as usize;
                        image.pixels[index..index + 4].copy_from_slice(&[value, value, value, 255]);
                    }
                }
                image
            };
            assert!(overlap_displacement(&repeating(0), &repeating(3), axis).is_err());
        }
        Ok(())
    }

    #[test]
    fn sparse_text_on_light_and_dark_pages_matches_both_axes() -> Result<()> {
        for axis in [Axis::Vertical, Axis::Horizontal] {
            for dark in [false, true] {
                let document = |offset: u32| {
                    let (width, height) = match axis {
                        Axis::Vertical => (512u32, 384u32),
                        Axis::Horizontal => (384, 512),
                    };
                    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
                    for y in 0..height {
                        for x in 0..width {
                            let (along, across) = match axis {
                                Axis::Vertical => (y + offset, x),
                                Axis::Horizontal => (x + offset, y),
                            };
                            let row = along % 24;
                            let seed = (along / 24).wrapping_mul(2_654_435_761)
                                ^ (across / 9).wrapping_mul(2_246_822_519);
                            let ink = (16..480).contains(&across)
                                && (4..16).contains(&row)
                                && across % 9 < 6
                                && seed.rotate_left(row) & (1 << (across % 9)) != 0;
                            let value = if ink ^ dark { 24 } else { 245 };
                            pixels.extend_from_slice(&[value, value, value, 255]);
                        }
                    }
                    CapturedImage {
                        width,
                        height,
                        pixels,
                    }
                };
                let first = document(0);
                let mut next = document(31);
                noisy(&mut next, 2);
                assert_eq!(overlap_displacement(&first, &next, axis)?, 31);
                assert_eq!(overlap_displacement(&next, &document(8), axis)?, -23);
            }
        }
        Ok(())
    }

    #[test]
    fn invalid_frames_still_fail_without_changing_the_stitch() -> Result<()> {
        let first = viewport(0, Axis::Vertical);
        let mut shot = LongShot::new(first.clone(), Axis::Vertical, false)?;
        let mut malformed = first.clone();
        malformed.pixels.pop();
        assert!(shot.push(malformed).is_err());
        let resized = CapturedImage {
            width: 32,
            height: 64,
            pixels: vec![255; 32 * 64 * 4],
        };
        assert!(shot.push(resized).is_err());
        assert_eq!(shot.previous.pixels, first.pixels);
        assert_eq!(shot.image().pixels, first.pixels);
        Ok(())
    }

    #[test]
    fn reversals_can_extend_before_origin_and_resume_forward_both_axes() -> Result<()> {
        for axis in [Axis::Vertical, Axis::Horizontal] {
            let mut shot = LongShot::new(viewport(20, axis), axis, false)?;
            assert_eq!(
                shot.push(viewport(0, axis))?,
                StitchUpdate::RolledBack { pixels: 20 }
            );
            assert_eq!(extent(shot.image(), axis), 84);
            assert_eq!(
                shot.push(viewport(30, axis))?,
                StitchUpdate::Advanced { pixels: 30 }
            );
            assert_eq!(extent(shot.image(), axis), 94);
            let expected = viewport(30, axis);
            for y in 0..64 {
                for x in 0..64 {
                    let (dx, dy) = match axis {
                        Axis::Vertical => (x, y + 30),
                        Axis::Horizontal => (x + 30, y),
                    };
                    let actual = ((dy * shot.image().width + dx) * 4) as usize;
                    let wanted = ((y * 64 + x) * 4) as usize;
                    assert_eq!(
                        &shot.image().pixels[actual..actual + 4],
                        &expected.pixels[wanted..wanted + 4]
                    );
                }
            }
        }
        Ok(())
    }

    #[test]
    fn shortcuts_require_new_presses_after_grace_and_do_not_repeat() {
        let defaults = HudKeys::default();
        for (keys, expected) in [
            (
                HudKeys {
                    escape: true,
                    ..defaults
                },
                HudCommand::Finish(LongShotAction::Cancel),
            ),
            (
                HudKeys {
                    right_button: true,
                    ..defaults
                },
                HudCommand::Finish(LongShotAction::Copy),
            ),
            (
                HudKeys {
                    enter: true,
                    ..defaults
                },
                HudCommand::Finish(LongShotAction::Copy),
            ),
            (
                HudKeys {
                    f2: true,
                    ..defaults
                },
                HudCommand::Finish(LongShotAction::Pin),
            ),
            (
                HudKeys {
                    s: true,
                    control: true,
                    ..defaults
                },
                HudCommand::Finish(LongShotAction::Save),
            ),
            (
                HudKeys {
                    space: true,
                    ..defaults
                },
                HudCommand::TogglePause,
            ),
        ] {
            let mut latch = ShortcutLatch::new(keys);
            assert_eq!(latch.poll(Duration::from_millis(199), keys), None);
            assert_eq!(latch.poll(Duration::from_millis(200), keys), None);
            assert_eq!(latch.poll(Duration::from_millis(201), defaults), None);
            assert_eq!(latch.poll(Duration::from_millis(202), keys), Some(expected));
            assert_eq!(latch.poll(Duration::from_secs(1), keys), None);
            let mut grace = ShortcutLatch::new(defaults);
            assert_eq!(grace.poll(Duration::from_millis(199), keys), None);
            assert_eq!(grace.poll(Duration::from_millis(200), keys), None);
            let mut ready = ShortcutLatch::new(defaults);
            assert_eq!(ready.poll(Duration::from_millis(200), keys), Some(expected));
        }
    }

    #[test]
    fn shortcut_modifiers_cannot_turn_held_keys_into_commands_or_modified_wheel_input() {
        let elapsed = Duration::from_secs(1);
        let defaults = HudKeys::default();
        let s = HudKeys {
            s: true,
            ..defaults
        };
        let mut latch = ShortcutLatch::new(defaults);
        assert_eq!(latch.poll(elapsed, s), None);
        assert_eq!(latch.poll(elapsed, HudKeys { control: true, ..s }), None);
        for keys in [
            HudKeys {
                f2: true,
                control: true,
                ..defaults
            },
            HudKeys {
                enter: true,
                alt: true,
                ..defaults
            },
            HudKeys {
                space: true,
                shift: true,
                ..defaults
            },
            HudKeys {
                s: true,
                control: true,
                meta: true,
                ..defaults
            },
        ] {
            let mut latch = ShortcutLatch::new(defaults);
            assert!(keys.blocks_scroll());
            assert_eq!(latch.poll(elapsed, keys), None);
            assert_eq!(
                latch.poll(
                    elapsed,
                    HudKeys {
                        control: false,
                        alt: false,
                        shift: false,
                        meta: false,
                        ..keys
                    }
                ),
                None
            );
        }
        assert!(!defaults.blocks_scroll());
    }

    #[test]
    fn paused_manual_frames_update_dimensions_and_exhaustion_waits_for_terminal_action()
    -> Result<()> {
        let mut shot = LongShot::new(viewport(0, Axis::Vertical), Axis::Vertical, false)?;
        let mut controls = CaptureControls::new(shot.image());
        controls.apply(HudCommand::TogglePause);
        assert!(controls.paused);
        let update = shot.push(viewport(20, Axis::Vertical))?;
        controls.captured(shot.image(), update, false, 1);
        assert_eq!(
            (controls.width, controls.height, controls.frames),
            (64, 84, 2)
        );
        assert!(controls.paused);
        controls.apply(HudCommand::TogglePause);
        assert!(!controls.paused);
        for _ in 0..2 {
            controls.captured(shot.image(), StitchUpdate::Unchanged, true, 10);
        }
        assert!(controls.paused);
        assert_eq!(controls.frames, 2);
        assert_eq!(controls.action, None);
        controls.apply(HudCommand::TogglePause);
        let update = shot.push(viewport(40, Axis::Vertical))?;
        controls.captured(shot.image(), update, true, 1);
        assert!(controls.paused);
        assert_eq!(
            (controls.height, controls.frames, controls.action),
            (104, 3, None)
        );
        controls.apply(HudCommand::Finish(LongShotAction::Save));
        controls.apply(HudCommand::Finish(LongShotAction::Copy));
        assert_eq!(controls.action, Some(LongShotAction::Save));
        Ok(())
    }

    #[test]
    fn cancellation_is_discard_and_overrides_pending_terminal_actions() {
        for action in [
            LongShotAction::Copy,
            LongShotAction::Pin,
            LongShotAction::Save,
        ] {
            let cancel = AtomicBool::new(false);
            assert_eq!(terminal_action(&cancel, Some(action)), Some(action));
            assert!(!cancel.load(Ordering::Acquire));
            cancel.store(true, Ordering::Release);
            assert_eq!(
                terminal_action(&cancel, Some(action)),
                Some(LongShotAction::Cancel)
            );
        }
        let cancel = AtomicBool::new(false);
        assert_eq!(terminal_action(&cancel, None), None);
        assert_eq!(
            terminal_action(&cancel, Some(LongShotAction::Cancel)),
            Some(LongShotAction::Cancel)
        );
        assert!(cancel.load(Ordering::Acquire));
        assert_eq!(terminal_action(&cancel, None), Some(LongShotAction::Cancel));
    }

    #[test]
    fn controls_hit_only_visible_scaled_button_bounds() {
        for scale in [0.75, 1., 1.5, 2.] {
            for control in HUD_CONTROLS {
                let bounds = control.bounds(scale);
                assert_eq!(
                    hud_hit(
                        scale,
                        Point {
                            x: bounds.x + bounds.width / 2.,
                            y: bounds.y + bounds.height / 2.
                        }
                    ),
                    Some(control)
                );
                assert_eq!(
                    hud_hit(
                        scale,
                        Point {
                            x: bounds.x + bounds.width,
                            y: bounds.y
                        }
                    ),
                    None
                );
            }
            assert_eq!(
                hud_hit(
                    scale,
                    Point {
                        x: -1.,
                        y: 70. * scale
                    }
                ),
                None
            );
            assert_eq!(
                hud_hit(
                    scale,
                    Point {
                        x: 200. * scale,
                        y: 20. * scale
                    }
                ),
                None
            );
        }
    }

    #[test]
    fn preview_uses_right_then_left_then_work_area_edge() {
        let work = Rect {
            x: 0.,
            y: 0.,
            width: 1280.,
            height: 800.,
        };
        let region = Rect {
            x: 300.,
            y: 100.,
            width: 400.,
            height: 300.,
        };
        let right = preview_bounds(region, work, 1.);
        assert_eq!(right.x, 712.);
        assert_eq!(right.y, region.y);
        assert_eq!((right.width, right.height), (200., 300.));

        let region = Rect { x: 700., ..region };
        let left = preview_bounds(region, work, 1.);
        assert_eq!(left.x, 488.);
        assert_eq!(left.y, region.y);
        assert!(left.x + left.width < region.x);

        let full = preview_bounds(work, work, 1.);
        assert_eq!(full.x + full.width, work.x + work.width);
        assert_eq!(full.height, 660.);
        assert!(full.y >= work.y && full.y + full.height <= work.y + work.height);
    }

    #[test]
    fn preview_layout_handles_high_dpi_negative_monitors_and_narrow_right_space() {
        let work = Rect {
            x: -2560.,
            y: -200.,
            width: 2560.,
            height: 1440.,
        };
        let region = Rect {
            x: -1700.,
            y: 900.,
            width: 800.,
            height: 200.,
        };
        let bounds = preview_bounds(region, work, 2.);
        assert_eq!(bounds.x, -876.);
        assert_eq!((bounds.width, bounds.height), (400., 480.));
        assert_eq!(bounds.y + bounds.height, work.y + work.height);
        let region = Rect {
            x: -1184.,
            y: 0.,
            ..region
        };
        let bounds = preview_bounds(region, work, 2.);
        assert_eq!(bounds.x, -360.);
        assert_eq!(bounds.width, 360.);
        let left = preview_bounds(
            Rect {
                x: -1183.,
                ..region
            },
            work,
            2.,
        );
        assert_eq!(left.x + left.width + 24., -1183.);
    }

    #[test]
    fn hud_prefers_below_the_selection_then_above() {
        let work = Rect {
            x: 0.,
            y: 0.,
            width: 1280.,
            height: 800.,
        };
        let region = Rect {
            x: 400.,
            y: 100.,
            width: 600.,
            height: 300.,
        };
        let below = hud_bounds(region, work, 1.);
        assert_eq!(below.y, 408.);
        assert_eq!(below.x, 360.);
        let region = Rect { y: 450., ..region };
        let above = hud_bounds(region, work, 1.);
        assert_eq!(above.y + above.height, region.y - 8.);
    }

    #[test]
    #[cfg(windows)]
    fn backdrop_keeps_selection_clear_with_dark_surround_and_blue_outer_border() -> Result<()> {
        let monitor = ui_rect(-100., -50., 100., 80.);
        let region = ui_rect(-80., -30., 40., 30.);
        let image = backdrop_canvas(region, monitor, 1.)?.image();
        let pixel = |x: usize, y: usize| &image.pixels[(y * image.width as usize + x) * 4..][..4];
        assert_eq!(pixel(5, 5), [0, 0, 0, 128]);
        assert_eq!(pixel(5, 30), [0, 0, 0, 128]);
        assert_eq!(pixel(80, 30), [0, 0, 0, 128]);
        assert_eq!(pixel(50, 70), [0, 0, 0, 128]);
        assert_eq!(pixel(20, 20), [0, 0, 0, 0]);
        assert_eq!(pixel(40, 35), [0, 0, 0, 0]);
        assert_eq!(pixel(59, 49), [0, 0, 0, 0]);
        assert_eq!(pixel(19, 35), [255, 119, 22, 255]);
        Ok(())
    }

    #[test]
    fn target_selection_rejects_current_process_and_missing_process() {
        assert!(!external_process(42, 42));
        assert!(!external_process(0, 42));
        assert!(external_process(43, 42));
    }

    #[test]
    fn cancelling_before_or_during_scrolling_returns_all_accepted_frames() -> Result<()> {
        let options = AutoScrollOptions {
            region: Rect {
                x: 0.,
                y: 0.,
                width: 64.,
                height: 64.,
            },
            target: Point::default(),
            axis: Axis::Vertical,
            channel: ScrollChannel::WindowMessage,
            steps: -1,
            settle: Duration::from_secs(1),
            max_frames: 10,
            auto_crop_on_scroll_back: false,
        };
        for accepted in [0, 1, 3] {
            let cancel = AtomicBool::new(false);
            let mut frames = 0;
            let mut previews = 0;
            let result = capture_frames(
                &options,
                &cancel,
                viewport(0, Axis::Vertical),
                |_, _| previews += 1,
                || {
                    if frames == accepted {
                        cancel.store(true, Ordering::Release);
                        return Ok(None);
                    }
                    frames += 1;
                    Ok(Some(viewport(frames * 20, Axis::Vertical)))
                },
            )?;
            assert!(cancel.load(Ordering::Acquire));
            assert_eq!(previews, accepted);
            assert_eq!(result.height, 64 + 20 * accepted);
        }
        let cancel = AtomicBool::new(true);
        let first = viewport(0, Axis::Vertical);
        let result = capture_frames(
            &options,
            &cancel,
            first.clone(),
            |_, _| panic!("unexpected preview"),
            || panic!("cancelled capture must not scroll"),
        )?;
        assert_eq!(result.pixels, first.pixels);
        Ok(())
    }

    #[test]
    #[cfg(windows)]
    fn hud_is_interactive_capture_excluded_nonactivating_and_destroyed_on_drop() -> Result<()> {
        let _desktop = crate::pin_window::NATIVE_WINDOW_TEST_LOCK.lock().unwrap();
        let hud = LongShotHud::new(
            Rect {
                x: 20.,
                y: 20.,
                width: 300.,
                height: 200.,
            },
            &viewport(0, Axis::Vertical),
        )?;
        let hwnd = hud.window.0;
        let layers = [hud.preview.window.0, hud._backdrop.0];
        unsafe {
            for layer in layers {
                let mut affinity = 0;
                GetWindowDisplayAffinity(layer, &mut affinity)?;
                assert_eq!(affinity, WDA_EXCLUDEFROMCAPTURE.0);
                let style = GetWindowLongPtrW(layer, GWL_EXSTYLE) as u32;
                for flag in [
                    WS_EX_TOPMOST,
                    WS_EX_LAYERED,
                    WS_EX_NOACTIVATE,
                    WS_EX_TRANSPARENT,
                ] {
                    assert_ne!(style & flag.0, 0);
                }
                assert_eq!(
                    SendMessageW(layer, WM_NCHITTEST, None, None).0,
                    HTTRANSPARENT as isize
                );
            }
            let mut affinity = 0;
            GetWindowDisplayAffinity(hwnd, &mut affinity)?;
            assert_eq!(affinity, WDA_EXCLUDEFROMCAPTURE.0);
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
            assert_eq!(ex & WS_EX_TRANSPARENT.0, 0);
            assert_ne!(ex & WS_EX_TOPMOST.0, 0);
            assert_ne!(ex & WS_EX_NOACTIVATE.0, 0);
            assert_eq!(
                SendMessageW(hwnd, WM_MOUSEACTIVATE, None, None).0,
                MA_NOACTIVATE as isize
            );
            assert!(
                ScrollTarget {
                    hwnd,
                    root: hwnd,
                    process: GetCurrentProcessId()
                }
                .validate()
                .is_err()
            );
        }
        drop(hud);
        assert!(!unsafe { IsWindow(Some(hwnd)).as_bool() });
        for layer in layers {
            assert!(!unsafe { IsWindow(Some(layer)).as_bool() });
        }
        Ok(())
    }

    #[test]
    #[cfg(windows)]
    fn hud_native_message_pump_dispatches_every_clickable_control() -> Result<()> {
        let _desktop = crate::pin_window::NATIVE_WINDOW_TEST_LOCK.lock().unwrap();
        for control in HUD_CONTROLS {
            let hud = LongShotHud::new(
                Rect {
                    x: 20.,
                    y: 20.,
                    width: 300.,
                    height: 200.,
                },
                &viewport(0, Axis::Vertical),
            )?;
            let bounds = control.bounds(hud.state.borrow().scale);
            let x = (bounds.x + bounds.width / 2.) as u16;
            let y = (bounds.y + bounds.height / 2.) as u16;
            let pointer = LPARAM((u32::from(x) | (u32::from(y) << 16)) as isize);
            unsafe {
                PostMessageW(Some(hud.window.0), WM_LBUTTONDOWN, WPARAM(0), pointer)?;
                PostMessageW(Some(hud.window.0), WM_LBUTTONUP, WPARAM(0), pointer)?;
            }
            let cancel = AtomicBool::new(false);
            hud.pump(
                &mut ShortcutLatch::new(HudKeys::read()),
                Duration::ZERO,
                &cancel,
            )?;
            match control.command() {
                HudCommand::TogglePause => assert!(hud.state.borrow().controls.paused),
                HudCommand::Finish(action) => {
                    assert_eq!(hud.state.borrow().controls.action, Some(action))
                }
            }
            assert_eq!(
                cancel.load(Ordering::Acquire),
                control == HudControl::Cancel
            );
        }
        Ok(())
    }

    #[test]
    #[cfg(windows)]
    fn hud_ignores_mouse_release_without_matching_press_and_accepts_cancel_while_paused()
    -> Result<()> {
        let _desktop = crate::pin_window::NATIVE_WINDOW_TEST_LOCK.lock().unwrap();
        let hud = LongShotHud::new(
            Rect {
                x: 20.,
                y: 20.,
                width: 300.,
                height: 200.,
            },
            &viewport(0, Axis::Vertical),
        )?;
        let bounds = HudControl::Copy.bounds(hud.state.borrow().scale);
        let x = (bounds.x + bounds.width / 2.) as u16;
        let y = (bounds.y + bounds.height / 2.) as u16;
        let pointer = LPARAM((u32::from(x) | (u32::from(y) << 16)) as isize);
        unsafe {
            SendMessageW(hud.window.0, WM_LBUTTONUP, None, Some(pointer));
            SendMessageW(hud.window.0, WM_LBUTTONDOWN, None, Some(pointer));
            SendMessageW(hud.window.0, WM_LBUTTONUP, None, Some(LPARAM(0)));
        }
        assert_eq!(hud.state.borrow().controls.action, None);
        hud.state
            .borrow_mut()
            .controls
            .apply(HudCommand::TogglePause);
        let cancel = AtomicBool::new(true);
        hud.pump(
            &mut ShortcutLatch::new(HudKeys::read()),
            Duration::ZERO,
            &cancel,
        )?;
        assert_eq!(
            hud.state.borrow().controls.action,
            Some(LongShotAction::Cancel)
        );
        Ok(())
    }
}
