use crate::Point;
use anyhow::{Context, Result, bail, ensure};
use orbment_core::config::VisionConfig;
use orbment_ui::{
    HookSubscription, HookThread, KeyboardHookEvent, MouseButton, MouseEventKind, MouseHookEvent,
    MouseHookHandler, MouseModifiers,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::Threading::*,
        UI::{Input::KeyboardAndMouse::*, Shell::*, WindowsAndMessaging::*},
    },
    core::PWSTR,
};

/// A recognizable application tag in addition to Windows' injected-input flag.
pub const REPLAY_TAG: usize = 0x4F52424D;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GestureState {
    WaitModifier,
    ModifierEntered,
    MouseButtonPressed,
    MouseDragging,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GesturePhase {
    Click,
    DoubleClick,
    Wheel,
    DragStarted,
    DragUpdated,
    DragFinished,
    Cancelled,
}
#[derive(Clone, Debug)]
pub struct GestureEvent {
    pub action: String,
    pub start: Point,
    pub current: Point,
    pub phase: GesturePhase,
    pub show_mask: bool,
}
#[derive(Clone, Copy, Debug)]
pub struct ReplayClick {
    pub button: MouseButton,
    pub position: Point,
    pub modifiers: MouseModifiers,
    pub release: bool,
    pub down: bool,
}
#[derive(Clone, Debug)]
pub enum GestureEffect {
    Action(GestureEvent),
    Replay(ReplayClick),
}
#[derive(Clone, Debug, Default)]
pub struct GestureOutcome {
    pub swallow: bool,
    pub effect: Option<GestureEffect>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gesture {
    Click,
    DoubleClick,
    Drag,
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
}
#[derive(Clone)]
struct Binding {
    modifiers: MouseModifiers,
    button: MouseButton,
    gesture: Gesture,
    action: String,
}
#[derive(Clone, Copy)]
struct Press {
    button: MouseButton,
    start: Point,
    modifiers: MouseModifiers,
    drag: Option<usize>,
    click: Option<usize>,
    double: Option<usize>,
}
#[derive(Clone, Copy)]
struct LastClick {
    button: MouseButton,
    point: Point,
    modifiers: MouseModifiers,
    time: u32,
}

/// Pure state transitions; neither window probing nor input injection runs here.
pub struct GestureMachine {
    pub state: GestureState,
    bindings: Vec<Binding>,
    threshold: f64,
    double_click_ms: u32,
    double_click_distance: f64,
    show_mask: bool,
    press: Option<Press>,
    last_click: Option<LastClick>,
    handed_off: Option<Press>,
}
impl GestureMachine {
    pub fn new(
        config: &VisionConfig,
        double_click_ms: u32,
        double_click_distance: f64,
    ) -> Result<Self> {
        let mut bindings: Vec<Binding> = Vec::new();
        for binding in &config.global_mouse.bindings {
            let mut modifiers = MouseModifiers::default();
            for modifier in &binding.modifiers {
                match modifier.as_str() {
                    "Ctrl" => modifiers.ctrl = true,
                    "Shift" => modifiers.shift = true,
                    "Alt" => modifiers.alt = true,
                    "Win" => modifiers.win = true,
                    _ => bail!("Unknown gesture modifier: {modifier}"),
                }
            }
            let button = match binding.button.as_str() {
                "Left" => MouseButton::Left,
                "Right" => MouseButton::Right,
                "Middle" => MouseButton::Middle,
                "X1" => MouseButton::X1,
                "X2" => MouseButton::X2,
                _ => bail!("Unknown gesture button: {}", binding.button),
            };
            let gesture = match binding.gesture.as_str() {
                "click" => Gesture::Click,
                "doubleClick" => Gesture::DoubleClick,
                "drag" => Gesture::Drag,
                "wheelUp" => Gesture::WheelUp,
                "wheelDown" => Gesture::WheelDown,
                "wheelLeft" => Gesture::WheelLeft,
                "wheelRight" => Gesture::WheelRight,
                _ => bail!("Unknown mouse gesture: {}", binding.gesture),
            };
            let wheel = matches!(
                gesture,
                Gesture::WheelUp | Gesture::WheelDown | Gesture::WheelLeft | Gesture::WheelRight
            );
            ensure!(
                wheel || button != MouseButton::Left || modifiers != MouseModifiers::default(),
                "Left mouse gestures require a modifier"
            );
            ensure!(
                !binding.action.trim().is_empty(),
                "Mouse gesture action is empty"
            );
            ensure!(
                !bindings
                    .iter()
                    .any(|other| (wheel || other.button == button)
                        && other.modifiers == modifiers
                        && other.gesture == gesture),
                "Duplicate mouse gesture binding"
            );
            bindings.push(Binding {
                modifiers,
                button,
                gesture,
                action: binding.action.clone(),
            });
        }
        Ok(Self {
            state: GestureState::WaitModifier,
            bindings,
            threshold: config.global_mouse.drag_threshold as f64,
            double_click_ms,
            double_click_distance,
            show_mask: config.global_mouse.show_mask_on_drag,
            press: None,
            last_click: None,
            handed_off: None,
        })
    }

    pub fn modifiers_changed(&mut self, modifiers: MouseModifiers) {
        if self.press.is_none() && self.handed_off.is_none() {
            self.state = if self
                .bindings
                .iter()
                .any(|binding| binding.modifiers == modifiers)
            {
                GestureState::ModifierEntered
            } else {
                GestureState::WaitModifier
            };
        }
    }

    fn action(
        &self,
        index: usize,
        start: Point,
        current: Point,
        phase: GesturePhase,
    ) -> GestureEffect {
        GestureEffect::Action(GestureEvent {
            action: self.bindings[index].action.clone(),
            start,
            current,
            phase,
            show_mask: self.show_mask,
        })
    }

    pub fn input(&mut self, event: MouseHookEvent, allowed: bool) -> GestureOutcome {
        let point = Point {
            x: event.x as f64,
            y: event.y as f64,
        };
        self.modifiers_changed(event.modifiers);
        if let Some(press) = self.handed_off {
            if event.kind == MouseEventKind::Up(press.button) {
                // Queue the real up behind the replayed down; a fast release must not overtake the worker.
                self.handed_off = None;
                self.state = GestureState::WaitModifier;
                return GestureOutcome {
                    swallow: true,
                    effect: Some(GestureEffect::Replay(ReplayClick {
                        button: press.button,
                        position: point,
                        modifiers: event.modifiers,
                        down: false,
                        release: true,
                    })),
                };
            }
            if event.kind == MouseEventKind::Down(press.button) {
                self.handed_off = None;
                self.state = GestureState::WaitModifier;
                let mut outcome = self.input(event, allowed);
                outcome.effect = Some(GestureEffect::Replay(ReplayClick {
                    button: press.button,
                    position: point,
                    modifiers: event.modifiers,
                    down: false,
                    release: true,
                }));
                return outcome;
            }
            return GestureOutcome::default();
        }
        if let Some(press) = self.press {
            match event.kind {
                MouseEventKind::Down(button) if button == press.button => {
                    let cancelled = self.interrupt();
                    let mut outcome = self.input(event, allowed);
                    outcome.effect = cancelled;
                    return outcome;
                }
                MouseEventKind::Move => {
                    let distance =
                        (point.x - press.start.x).abs() + (point.y - press.start.y).abs();
                    if let Some(index) = press.drag {
                        if self.state == GestureState::MouseDragging || distance > self.threshold {
                            let phase = if self.state == GestureState::MouseDragging {
                                GesturePhase::DragUpdated
                            } else {
                                GesturePhase::DragStarted
                            };
                            self.state = GestureState::MouseDragging;
                            return GestureOutcome {
                                swallow: false,
                                effect: Some(self.action(index, press.start, point, phase)),
                            };
                        }
                    } else if distance > self.threshold {
                        self.press = None;
                        self.handed_off = Some(press);
                        self.last_click = None;
                        self.state = GestureState::WaitModifier;
                        return GestureOutcome {
                            swallow: false,
                            effect: Some(GestureEffect::Replay(ReplayClick {
                                button: press.button,
                                position: press.start,
                                modifiers: press.modifiers,
                                down: true,
                                release: false,
                            })),
                        };
                    }
                    return GestureOutcome {
                        swallow: false,
                        effect: None,
                    };
                }
                MouseEventKind::Up(button) if button == press.button => {
                    let dragging = self.state == GestureState::MouseDragging;
                    self.press = None;
                    self.state = GestureState::WaitModifier;
                    let effect = if dragging {
                        self.last_click = None;
                        press.drag.map(|index| {
                            self.action(index, press.start, point, GesturePhase::DragFinished)
                        })
                    } else {
                        let double = self.last_click.is_some_and(|last| {
                            last.button == button
                                && last.modifiers == press.modifiers
                                && event.time_ms.wrapping_sub(last.time) <= self.double_click_ms
                                && (point.x - last.point.x).abs() + (point.y - last.point.y).abs()
                                    <= self.double_click_distance
                        });
                        self.last_click = Some(LastClick {
                            button,
                            point,
                            modifiers: press.modifiers,
                            time: event.time_ms,
                        });
                        if double && press.double.is_some() {
                            self.last_click = None;
                            press.double.map(|index| {
                                self.action(index, press.start, point, GesturePhase::DoubleClick)
                            })
                        } else if let Some(index) = press.click {
                            Some(self.action(index, press.start, point, GesturePhase::Click))
                        } else {
                            Some(GestureEffect::Replay(ReplayClick {
                                button,
                                position: press.start,
                                modifiers: press.modifiers,
                                down: true,
                                release: true,
                            }))
                        }
                    };
                    return GestureOutcome {
                        swallow: true,
                        effect,
                    };
                }
                _ => return GestureOutcome::default(),
            }
        }
        if !allowed {
            self.state = GestureState::WaitModifier;
            return GestureOutcome::default();
        }
        match event.kind {
            MouseEventKind::Down(button) => {
                let find = |gesture| {
                    self.bindings.iter().position(|binding| {
                        binding.button == button
                            && binding.modifiers == event.modifiers
                            && binding.gesture == gesture
                    })
                };
                let press = Press {
                    button,
                    start: point,
                    modifiers: event.modifiers,
                    drag: find(Gesture::Drag),
                    click: find(Gesture::Click),
                    double: find(Gesture::DoubleClick),
                };
                if press.drag.is_some() || press.click.is_some() || press.double.is_some() {
                    self.press = Some(press);
                    self.state = GestureState::MouseButtonPressed;
                    return GestureOutcome {
                        swallow: true,
                        effect: None,
                    };
                }
            }
            MouseEventKind::Wheel { delta, horizontal } if delta != 0 => {
                let gesture = match (horizontal, delta > 0) {
                    (false, true) => Gesture::WheelUp,
                    (false, false) => Gesture::WheelDown,
                    (true, true) => Gesture::WheelRight,
                    (true, false) => Gesture::WheelLeft,
                };
                if let Some(index) = self.bindings.iter().position(|binding| {
                    binding.modifiers == event.modifiers && binding.gesture == gesture
                }) {
                    return GestureOutcome {
                        swallow: true,
                        effect: Some(self.action(index, point, point, GesturePhase::Wheel)),
                    };
                }
            }
            _ => {}
        }
        GestureOutcome::default()
    }

    pub fn interrupt(&mut self) -> Option<GestureEffect> {
        let effect = self.press.and_then(|press| {
            if self.state == GestureState::MouseDragging {
                press.drag.map(|index| {
                    self.action(index, press.start, press.start, GesturePhase::Cancelled)
                })
            } else {
                None
            }
        });
        self.press = None;
        self.last_click = None;
        self.state = GestureState::WaitModifier;
        effect
    }

    pub fn cancel(&mut self) -> Option<ReplayClick> {
        let press = self.press.take();
        self.state = GestureState::WaitModifier;
        self.last_click = None;
        let handed_off = self.handed_off.take();
        press
            .map(|press| ReplayClick {
                button: press.button,
                position: press.start,
                modifiers: press.modifiers,
                down: true,
                release: false,
            })
            .or_else(|| {
                handed_off.map(|press| ReplayClick {
                    button: press.button,
                    position: press.start,
                    modifiers: press.modifiers,
                    down: false,
                    release: true,
                })
            })
    }
}

// A seqlock publishes the worker's point permission without locking the hook thread.
struct Permission {
    sequence: AtomicU32,
    allowed: AtomicBool,
    x: AtomicI32,
    y: AtomicI32,
    time: AtomicU32,
}
impl Permission {
    fn publish(&self, point: POINT, allowed: bool, time: u32) {
        self.sequence.fetch_add(1, Ordering::SeqCst);
        self.x.store(point.x, Ordering::SeqCst);
        self.y.store(point.y, Ordering::SeqCst);
        self.allowed.store(allowed, Ordering::SeqCst);
        self.time.store(time, Ordering::SeqCst);
        self.sequence.fetch_add(1, Ordering::SeqCst);
    }
    fn allows(&self, event: MouseHookEvent) -> bool {
        let before = self.sequence.load(Ordering::SeqCst);
        let age = event.time_ms.wrapping_sub(self.time.load(Ordering::SeqCst)) as i32;
        let allowed = self.allowed.load(Ordering::SeqCst)
            && (event.x as i64 - self.x.load(Ordering::SeqCst) as i64).abs() <= 8
            && (event.y as i64 - self.y.load(Ordering::SeqCst) as i64).abs() <= 8
            && (-15..=250).contains(&age);
        allowed && before & 1 == 0 && before == self.sequence.load(Ordering::SeqCst)
    }
}
enum WorkerMessage {
    Effect(GestureEffect),
    Probe,
    Stop,
}

struct Handler {
    machine: GestureMachine,
    permission: Arc<Permission>,
    sender: Sender<WorkerMessage>,
    active: Arc<AtomicBool>,
    armed: Arc<AtomicBool>,
    last_probe: u32,
}
impl Handler {
    fn update_armed(&mut self, time_ms: u32, modifiers: MouseModifiers) {
        let armed = self.machine.press.is_none()
            && self.machine.handed_off.is_none()
            && self
                .machine
                .bindings
                .iter()
                .any(|binding| binding.modifiers == modifiers);
        let previously_armed = self.armed.swap(armed, Ordering::AcqRel);
        if armed && (!previously_armed || time_ms.wrapping_sub(self.last_probe) >= 40) {
            self.last_probe = time_ms;
            let _ = self.sender.send(WorkerMessage::Probe);
        }
    }
}
impl MouseHookHandler for Handler {
    fn on_mouse(&mut self, event: MouseHookEvent) -> bool {
        if !self.active.load(Ordering::Acquire) {
            return false;
        }
        let outcome = self.machine.input(event, self.permission.allows(event));
        if let Some(effect) = outcome.effect {
            let _ = self.sender.send(WorkerMessage::Effect(effect));
        }
        self.update_armed(event.time_ms, event.modifiers);
        outcome.swallow
    }
    fn on_keyboard(&mut self, event: KeyboardHookEvent) {
        self.machine.modifiers_changed(event.modifiers);
        self.update_armed(event.time_ms, event.modifiers);
    }
    fn on_unmount(&mut self) {
        if let Some(press) = self.machine.press
            && self.machine.state == GestureState::MouseDragging
            && let Some(index) = press.drag
        {
            let event =
                self.machine
                    .action(index, press.start, press.start, GesturePhase::Cancelled);
            let _ = self.sender.send(WorkerMessage::Effect(event));
        }
        if let Some(replay) = self.machine.cancel() {
            let _ = self
                .sender
                .send(WorkerMessage::Effect(GestureEffect::Replay(replay)));
        }
        self.armed.store(false, Ordering::Release);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ExclusionKey {
    window: isize,
    process: u32,
    foreground: isize,
    bounds: [i32; 4],
    monitor: isize,
}
#[derive(Default)]
struct ExclusionCache {
    previous: Option<(ExclusionKey, bool, Instant)>,
}
impl ExclusionCache {
    fn refresh(&mut self, config: &VisionConfig, permission: &Permission) {
        let mut point = POINT::default();
        let allowed = (|| -> Result<bool> {
            unsafe {
                GetCursorPos(&mut point)?;
                let window = WindowFromPoint(point);
                ensure!(!window.is_invalid(), "No gesture target window");
                let mut bounds = RECT::default();
                GetWindowRect(GetAncestor(window, GA_ROOT), &mut bounds)?;
                let mut process = 0;
                GetWindowThreadProcessId(window, Some(&mut process));
                let key = ExclusionKey {
                    window: window.0 as isize,
                    process,
                    foreground: GetForegroundWindow().0 as isize,
                    bounds: [bounds.left, bounds.top, bounds.right, bounds.bottom],
                    monitor: MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST).0 as isize,
                };
                if let Some((previous, allowed, checked)) = self.previous
                    && previous == key
                    && checked.elapsed() < Duration::from_secs(1)
                {
                    return Ok(allowed);
                }
                let allowed = !is_excluded(
                    config,
                    Point {
                        x: point.x as f64,
                        y: point.y as f64,
                    },
                )?;
                self.previous = Some((key, allowed, Instant::now()));
                Ok(allowed)
            }
        })()
        .unwrap_or(false);
        let time = unsafe { windows::Win32::System::SystemInformation::GetTickCount64() } as u32;
        permission.publish(point, allowed, time);
    }
}

pub struct GlobalMouseRegistration {
    subscription: Option<HookSubscription>,
    active: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    events: Receiver<GestureEvent>,
    sender: Sender<WorkerMessage>,
}
impl GlobalMouseRegistration {
    pub fn install(
        hooks: &mut HookThread,
        config: &VisionConfig,
        wake: impl Fn() + Send + 'static,
    ) -> Result<Self> {
        ensure!(
            config.enabled && !config.global_mouse.bindings.is_empty(),
            "Global mouse gestures are disabled"
        );
        let machine = GestureMachine::new(config, unsafe { GetDoubleClickTime() }, unsafe {
            GetSystemMetrics(SM_CXDOUBLECLK) + GetSystemMetrics(SM_CYDOUBLECLK)
        }
            as f64)?;
        let permission = Arc::new(Permission {
            sequence: AtomicU32::new(0),
            allowed: AtomicBool::new(false),
            x: AtomicI32::new(0),
            y: AtomicI32::new(0),
            time: AtomicU32::new(0),
        });
        let active = Arc::new(AtomicBool::new(true));
        let armed = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::channel::<WorkerMessage>();
        let (events_tx, events) = mpsc::channel();
        let worker_armed = armed.clone();
        let worker_permission = permission.clone();
        let config = config.clone();
        let worker = thread::Builder::new()
            .name("orbment-gesture-context".into())
            .spawn(move || {
                let mut cache = ExclusionCache::default();
                cache.refresh(&config, &worker_permission);
                let mut refreshed = Instant::now();
                loop {
                    let message = if worker_armed.load(Ordering::Acquire) {
                        match receiver.recv_timeout(Duration::from_millis(100)) {
                            Ok(message) => message,
                            Err(mpsc::RecvTimeoutError::Timeout) => WorkerMessage::Probe,
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        }
                    } else {
                        match receiver.recv() {
                            Ok(message) => message,
                            Err(_) => break,
                        }
                    };
                    match message {
                        WorkerMessage::Effect(GestureEffect::Replay(replay)) => {
                            if let Err(error) = replay_click(replay) {
                                tracing::warn!(%error, "Replaying swallowed click");
                            }
                        }
                        WorkerMessage::Effect(GestureEffect::Action(event)) => {
                            if events_tx.send(event).is_ok() {
                                wake();
                            }
                        }
                        WorkerMessage::Probe => {
                            if worker_armed.load(Ordering::Acquire)
                                && refreshed.elapsed() >= Duration::from_millis(30)
                            {
                                cache.refresh(&config, &worker_permission);
                                refreshed = Instant::now();
                            }
                        }
                        WorkerMessage::Stop => break,
                    }
                }
            })
            .context("Starting gesture context worker")?;
        let handler = Handler {
            machine,
            permission,
            sender: sender.clone(),
            active: active.clone(),
            armed,
            last_probe: 0,
        };
        match hooks.acquire_mouse(Box::new(handler)) {
            Ok(subscription) => Ok(Self {
                subscription: Some(subscription),
                active,
                worker: Some(worker),
                events,
                sender,
            }),
            Err(error) => {
                active.store(false, Ordering::Release);
                let _ = sender.send(WorkerMessage::Stop);
                let _ = worker.join();
                Err(error)
            }
        }
    }
    pub fn take_events(&self) -> Vec<GestureEvent> {
        self.events.try_iter().collect()
    }
    pub fn uninstall(&mut self, hooks: &mut HookThread) -> Result<()> {
        if let Some(subscription) = self.subscription {
            hooks.release_mouse(subscription)?;
            self.subscription = None;
        }
        self.active.store(false, Ordering::Release);
        let _ = self.sender.send(WorkerMessage::Stop);
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("Gesture context worker panicked"))?;
        }
        Ok(())
    }
}
impl Drop for GlobalMouseRegistration {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
        let _ = self.sender.send(WorkerMessage::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn query_process_name(pid: u32) -> Option<String> {
    unsafe {
        // Protected processes may deny image queries; window checks still apply.
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buffer = vec![0u16; 32768];
        let mut length = buffer.len() as u32;
        let result = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        );
        let _ = CloseHandle(process);
        result.ok()?;
        let path = String::from_utf16_lossy(&buffer[..length as usize]);
        Some(path.rsplit(['\\', '/']).next().unwrap_or(&path).to_owned())
    }
}

fn is_fullscreen_game(
    process_name: Option<&str>,
    window_class: &str,
    style: u32,
    maximized: bool,
    window_rect: RECT,
    monitor_rect: RECT,
    notification_state: impl FnOnce() -> Result<QUERY_USER_NOTIFICATION_STATE>,
) -> Result<bool> {
    let browser = process_name.is_some_and(|name| {
        [
            "chrome.exe",
            "msedge.exe",
            "brave.exe",
            "opera.exe",
            "firefox.exe",
        ]
        .iter()
        .any(|browser| name.eq_ignore_ascii_case(browser))
    }) || ["Chrome_WidgetWin_1", "MozillaWindowClass"]
        .iter()
        .any(|browser| window_class.eq_ignore_ascii_case(browser));
    let has_caption = style & WS_CAPTION.0 != 0;
    // Video/WebGL can set the global D3D state even in ordinary browser windows.
    if browser || (maximized && has_caption) {
        return Ok(false);
    }
    Ok((!has_caption && window_rect == monitor_rect)
        || notification_state()? == QUNS_RUNNING_D3D_FULL_SCREEN)
}

/// Worker-only exclusion boundary: no process handles or shell calls in the LL callback.
pub fn is_excluded(config: &VisionConfig, point: Point) -> Result<bool> {
    unsafe {
        let hwnd = WindowFromPoint(POINT {
            x: point.x as i32,
            y: point.y as i32,
        });
        if hwnd.is_invalid() {
            return Ok(true);
        }
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let current_pid = GetCurrentProcessId();
        if pid == current_pid {
            return Ok(false);
        }
        let process_name =
            if config.disable_in_fullscreen_game || !config.ignore_process_list.is_empty() {
                query_process_name(pid)
            } else {
                None
            };
        if process_name.as_deref().is_some_and(|name| {
            config
                .ignore_process_list
                .iter()
                .any(|excluded| excluded.eq_ignore_ascii_case(name))
        }) {
            return Ok(true);
        }
        if config.disable_in_fullscreen_game {
            let foreground = GetAncestor(GetForegroundWindow(), GA_ROOT);
            if foreground.is_invalid()
                || foreground == GetShellWindow()
                || foreground == GetDesktopWindow()
            {
                return Ok(false);
            }
            let mut foreground_pid = 0;
            GetWindowThreadProcessId(foreground, Some(&mut foreground_pid));
            // The shell's global fullscreen state must not suppress our capture windows.
            if foreground_pid == current_pid {
                return Ok(false);
            }
            let foreground_process_name = if foreground_pid == pid {
                process_name
            } else {
                query_process_name(foreground_pid)
            };
            let mut class = [0u16; 256];
            let class_len = GetClassNameW(foreground, &mut class).max(0) as usize;
            let window_class = String::from_utf16_lossy(&class[..class_len]);
            let mut window_rect = RECT::default();
            GetWindowRect(foreground, &mut window_rect)?;
            let monitor = MonitorFromPoint(
                POINT {
                    x: point.x as i32,
                    y: point.y as i32,
                },
                MONITOR_DEFAULTTONEAREST,
            );
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            ensure!(
                GetMonitorInfoW(monitor, &mut info).as_bool(),
                "Reading gesture monitor bounds"
            );
            return is_fullscreen_game(
                foreground_process_name.as_deref(),
                &window_class,
                GetWindowLongPtrW(foreground, GWL_STYLE) as u32,
                IsZoomed(foreground).as_bool(),
                window_rect,
                info.rcMonitor,
                || Ok(SHQueryUserNotificationState()?),
            );
        }
    }
    Ok(false)
}

pub fn replay_click(replay: ReplayClick) -> Result<()> {
    unsafe {
        let mut cursor = POINT::default();
        GetCursorPos(&mut cursor)?;
        let mut inputs = Vec::with_capacity(16);
        let modifiers = [
            (VK_LCONTROL, VK_RCONTROL, replay.modifiers.ctrl),
            (VK_LSHIFT, VK_RSHIFT, replay.modifiers.shift),
            (VK_LMENU, VK_RMENU, replay.modifiers.alt),
            (VK_LWIN, VK_RWIN, replay.modifiers.win),
        ];
        let mut restore = Vec::new();
        for (left_key, right_key, wanted) in modifiers {
            let left_down = GetAsyncKeyState(left_key.0 as i32) < 0;
            let right_down = GetAsyncKeyState(right_key.0 as i32) < 0;
            if wanted && !left_down && !right_down {
                inputs.push(key_input(left_key, false));
                restore.push(key_input(left_key, true));
            } else if !wanted {
                for (key, down) in [(left_key, left_down), (right_key, right_down)] {
                    if down {
                        inputs.push(key_input(key, true));
                        restore.push(key_input(key, false));
                    }
                }
            }
        }
        let left = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let top = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let width = (GetSystemMetrics(SM_CXVIRTUALSCREEN) - 1).max(1);
        let height = (GetSystemMetrics(SM_CYVIRTUALSCREEN) - 1).max(1);
        let movement = |x: i32, y: i32| {
            mouse_input(
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                (((x as i64 - left as i64) * 65535) / width as i64) as i32,
                (((y as i64 - top as i64) * 65535) / height as i64) as i32,
                0,
            )
        };
        inputs.push(movement(replay.position.x as i32, replay.position.y as i32));
        let (down, up, data) = match replay.button {
            MouseButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, 0),
            MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, 0),
            MouseButton::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 0),
            MouseButton::X1 => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, XBUTTON1 as u32),
            MouseButton::X2 => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, XBUTTON2 as u32),
        };
        if replay.down {
            inputs.push(mouse_input(down, 0, 0, data));
        }
        if replay.release {
            inputs.push(mouse_input(up, 0, 0, data));
        }
        inputs.push(movement(cursor.x, cursor.y));
        inputs.extend(restore);
        ensure!(
            SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) == inputs.len() as u32,
            "SendInput did not replay the complete swallowed click"
        );
    }
    Ok(())
}
fn key_input(key: VIRTUAL_KEY, up: bool) -> INPUT {
    let extended = matches!(key, VK_RCONTROL | VK_RMENU | VK_LWIN | VK_RWIN);
    let flags = (if up {
        KEYEVENTF_KEYUP
    } else {
        KEYBD_EVENT_FLAGS(0)
    }) | (if extended {
        KEYEVENTF_EXTENDEDKEY
    } else {
        KEYBD_EVENT_FLAGS(0)
    });
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: key,
                dwFlags: flags,
                dwExtraInfo: REPLAY_TAG,
                ..Default::default()
            },
        },
    }
}
fn mouse_input(flags: MOUSE_EVENT_FLAGS, x: i32, y: i32, data: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: x,
                dy: y,
                mouseData: data,
                dwFlags: flags,
                dwExtraInfo: REPLAY_TAG,
                ..Default::default()
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MONITOR_BOUNDS: RECT = RECT {
        left: -1920,
        top: 0,
        right: 0,
        bottom: 1080,
    };

    #[test]
    fn browsers_are_not_games_even_with_full_monitor_bounds_or_d3d_state() {
        for (process_name, window_class) in [
            (Some("chrome.exe"), "OtherClass"),
            (Some("msedge.exe"), "OtherClass"),
            (Some("brave.exe"), "OtherClass"),
            (Some("opera.exe"), "OtherClass"),
            (Some("firefox.exe"), "OtherClass"),
            (Some("CHROME.EXE"), "OtherClass"),
            (None, "Chrome_WidgetWin_1"),
            (None, "MozillaWindowClass"),
            (Some("unknown.exe"), "Chrome_WidgetWin_1"),
        ] {
            for (style, maximized) in [(0, false), (WS_CAPTION.0, false), (WS_CAPTION.0, true)] {
                for window_rect in [
                    MONITOR_BOUNDS,
                    RECT {
                        bottom: 700,
                        ..MONITOR_BOUNDS
                    },
                ] {
                    assert!(
                        !is_fullscreen_game(
                            process_name,
                            window_class,
                            style,
                            maximized,
                            window_rect,
                            MONITOR_BOUNDS,
                            || panic!("Browser windows must bypass the global D3D state"),
                        )
                        .unwrap(),
                        "{process_name:?} / {window_class}",
                    );
                }
            }
        }
    }

    #[test]
    fn captioned_maximized_windows_are_not_fullscreen_games() {
        for window_rect in [
            MONITOR_BOUNDS,
            RECT {
                left: MONITOR_BOUNDS.left - 8,
                top: MONITOR_BOUNDS.top - 8,
                right: MONITOR_BOUNDS.right + 8,
                bottom: MONITOR_BOUNDS.bottom + 8,
            },
        ] {
            assert!(
                !is_fullscreen_game(
                    Some("editor.exe"),
                    "Editor",
                    WS_CAPTION.0 | WS_THICKFRAME.0,
                    true,
                    window_rect,
                    MONITOR_BOUNDS,
                    || panic!("Standard maximized windows must bypass the global D3D state"),
                )
                .unwrap()
            );
        }
    }

    #[test]
    fn borderless_games_must_match_the_entire_physical_monitor() {
        for (window_rect, style, maximized, expected) in [
            (MONITOR_BOUNDS, 0, false, true),
            (MONITOR_BOUNDS, 0, true, true),
            (MONITOR_BOUNDS, WS_CAPTION.0, false, false),
            (
                RECT {
                    bottom: MONITOR_BOUNDS.bottom - 40,
                    ..MONITOR_BOUNDS
                },
                0,
                false,
                false,
            ),
            (
                RECT {
                    right: MONITOR_BOUNDS.right - 1,
                    ..MONITOR_BOUNDS
                },
                0,
                false,
                false,
            ),
            (
                RECT {
                    left: MONITOR_BOUNDS.left - 8,
                    top: MONITOR_BOUNDS.top - 8,
                    right: MONITOR_BOUNDS.right + 8,
                    bottom: MONITOR_BOUNDS.bottom + 8,
                },
                0,
                false,
                false,
            ),
        ] {
            assert_eq!(
                is_fullscreen_game(
                    Some("game.exe"),
                    "Game",
                    style,
                    maximized,
                    window_rect,
                    MONITOR_BOUNDS,
                    || Ok(QUNS_ACCEPTS_NOTIFICATIONS),
                )
                .unwrap(),
                expected,
                "{window_rect:?}, style={style}, maximized={maximized}",
            );
        }
    }

    #[test]
    fn exclusive_d3d_games_do_not_require_borderless_window_bounds() {
        for style in [0, WS_CAPTION.0] {
            for (notification_state, expected) in [
                (QUNS_RUNNING_D3D_FULL_SCREEN, true),
                (QUNS_ACCEPTS_NOTIFICATIONS, false),
                (QUNS_BUSY, false),
            ] {
                assert_eq!(
                    is_fullscreen_game(
                        None,
                        "Game",
                        style,
                        false,
                        RECT {
                            bottom: 720,
                            ..MONITOR_BOUNDS
                        },
                        MONITOR_BOUNDS,
                        || Ok(notification_state),
                    )
                    .unwrap(),
                    expected,
                );
            }
        }
    }

    fn machine() -> GestureMachine {
        let mut config = VisionConfig::default();
        config
            .global_mouse
            .bindings
            .push(orbment_core::config::MouseGestureBinding {
                modifiers: vec!["Ctrl".into()],
                button: "Left".into(),
                gesture: "drag".into(),
                action: "vision.pin_last".into(),
            });
        GestureMachine::new(&config, 500, 8.0).unwrap()
    }
    fn event(kind: MouseEventKind, x: i32, y: i32) -> MouseHookEvent {
        MouseHookEvent {
            kind,
            x,
            y,
            time_ms: 100,
            modifiers: MouseModifiers {
                ctrl: true,
                ..Default::default()
            },
        }
    }
    #[test]
    fn modifier_press_threshold_drag_release_transitions() {
        let mut machine = machine();
        assert_eq!(machine.state, GestureState::WaitModifier);
        machine.modifiers_changed(event(MouseEventKind::Move, 0, 0).modifiers);
        assert_eq!(machine.state, GestureState::ModifierEntered);
        assert!(
            machine
                .input(event(MouseEventKind::Down(MouseButton::Left), 10, 10), true)
                .swallow
        );
        assert_eq!(machine.state, GestureState::MouseButtonPressed);
        assert!(
            machine
                .input(event(MouseEventKind::Move, 14, 14), true)
                .effect
                .is_none()
        );
        let started = machine.input(event(MouseEventKind::Move, 15, 14), true);
        assert!(matches!(
            started.effect,
            Some(GestureEffect::Action(GestureEvent {
                phase: GesturePhase::DragStarted,
                ..
            }))
        ));
        assert_eq!(machine.state, GestureState::MouseDragging);
        let finished = machine.input(event(MouseEventKind::Up(MouseButton::Left), 40, 50), true);
        assert!(finished.swallow);
        assert!(matches!(
            finished.effect,
            Some(GestureEffect::Action(GestureEvent {
                phase: GesturePhase::DragFinished,
                ..
            }))
        ));
        assert_eq!(machine.state, GestureState::WaitModifier);
    }
    #[test]
    fn untriggered_drag_replays_original_swallowed_click() {
        let mut machine = machine();
        assert!(
            machine
                .input(
                    event(MouseEventKind::Down(MouseButton::Left), -120, 42),
                    true
                )
                .swallow
        );
        let outcome = machine.input(event(MouseEventKind::Up(MouseButton::Left), -118, 43), true);
        assert!(outcome.swallow);
        let Some(GestureEffect::Replay(replay)) = outcome.effect else {
            panic!("Missing click replay");
        };
        assert_eq!(replay.button, MouseButton::Left);
        assert_eq!((replay.position.x, replay.position.y), (-120.0, 42.0));
        assert!(replay.modifiers.ctrl && replay.down && replay.release);
        assert_eq!(machine.state, GestureState::WaitModifier);
        assert!(
            !machine
                .input(event(MouseEventKind::Down(MouseButton::Left), 0, 0), false)
                .swallow
        );
    }
    #[test]
    fn click_binding_hands_drag_back_with_ordered_down_and_up() {
        let mut config = VisionConfig::default();
        config
            .global_mouse
            .bindings
            .push(orbment_core::config::MouseGestureBinding {
                modifiers: vec!["Ctrl".into()],
                button: "Left".into(),
                gesture: "click".into(),
                action: "vision.capture_screen".into(),
            });
        let mut machine = GestureMachine::new(&config, 500, 8.0).unwrap();
        assert!(
            machine
                .input(event(MouseEventKind::Down(MouseButton::Left), 5, 6), true)
                .swallow
        );
        let moved = machine.input(event(MouseEventKind::Move, 50, 60), true);
        assert!(!moved.swallow);
        let Some(GestureEffect::Replay(down)) = moved.effect else {
            panic!("Missing original down");
        };
        assert!(down.down && !down.release);
        assert_eq!((down.position.x, down.position.y), (5.0, 6.0));
        let released = machine.input(event(MouseEventKind::Up(MouseButton::Left), 70, 80), true);
        assert!(released.swallow);
        let Some(GestureEffect::Replay(up)) = released.effect else {
            panic!("Missing ordered up");
        };
        assert!(!up.down && up.release);
        assert_eq!((up.position.x, up.position.y), (70.0, 80.0));
        assert_eq!(machine.state, GestureState::WaitModifier);
    }

    #[test]
    fn fresh_down_cancels_stale_drag_without_completing_capture() {
        let mut machine = machine();
        machine.input(event(MouseEventKind::Down(MouseButton::Left), 10, 10), true);
        machine.input(event(MouseEventKind::Move, 30, 30), true);
        let fresh = machine.input(
            event(MouseEventKind::Down(MouseButton::Left), 100, 100),
            true,
        );
        assert!(fresh.swallow);
        assert!(matches!(
            fresh.effect,
            Some(GestureEffect::Action(GestureEvent {
                phase: GesturePhase::Cancelled,
                ..
            }))
        ));
        assert_eq!(machine.state, GestureState::MouseButtonPressed);
        let released = machine.input(event(MouseEventKind::Up(MouseButton::Left), 100, 100), true);
        let Some(GestureEffect::Replay(replay)) = released.effect else {
            panic!("Missing fresh click");
        };
        assert_eq!((replay.position.x, replay.position.y), (100.0, 100.0));
        assert!(replay.down && replay.release);
    }
}
