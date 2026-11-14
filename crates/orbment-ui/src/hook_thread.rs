use crate::event_loop::{CLIPBOARD_MESSAGE, MUTE_MESSAGE, SEARCH_MESSAGE};
use anyhow::{Context, Result, anyhow};
use std::{
    cell::{Cell, RefCell},
    marker::PhantomData,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
};
use windows::Win32::{
    Foundation::*,
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

const MUTE: u8 = 1;
const WIN_V: u8 = 2;
const DOUBLE_CTRL: u8 = 4;
const HOOK_COMMAND: u32 = WM_APP + 73;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MouseModifiers {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub win: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseEventKind {
    Move,
    Down(MouseButton),
    Up(MouseButton),
    Wheel { delta: i16, horizontal: bool },
}

#[derive(Clone, Copy, Debug)]
pub struct MouseHookEvent {
    pub kind: MouseEventKind,
    pub x: i32,
    pub y: i32,
    pub time_ms: u32,
    pub modifiers: MouseModifiers,
}

#[derive(Clone, Copy, Debug)]
pub struct KeyboardHookEvent {
    pub virtual_key: u32,
    pub scan_code: u32,
    pub down: bool,
    pub time_ms: u32,
    pub modifiers: MouseModifiers,
}

/// Runs on the hook thread. Only memory operations and asynchronous delivery belong here.
pub trait MouseHookHandler: Send {
    fn on_mouse(&mut self, event: MouseHookEvent) -> bool;
    fn on_keyboard(&mut self, _event: KeyboardHookEvent) {}
    fn on_unmount(&mut self) {}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HookSubscription(u64);
type KeyboardObserver = Box<dyn FnMut(KeyboardHookEvent) + Send>;
enum HookCommand {
    AddMouse(HookSubscription, Box<dyn MouseHookHandler>),
    RemoveMouse(HookSubscription),
    AddKeyboard(HookSubscription, KeyboardObserver),
    RemoveKeyboard(HookSubscription),
}
type CommandRequest = (
    HookCommand,
    mpsc::SyncSender<std::result::Result<(), String>>,
);
thread_local! {
    static TARGET: Cell<usize> = const { Cell::new(0) };
    static FLAGS: RefCell<Option<Arc<AtomicU8>>> = const { RefCell::new(None) };
    static MUTE_DOWN: Cell<bool> = const { Cell::new(false) };
    static V_DOWN: Cell<bool> = const { Cell::new(false) };
    static CTRL_DOWN: Cell<bool> = const { Cell::new(false) };
    static CTRL_USED: Cell<bool> = const { Cell::new(false) };
    static CTRL_LAST: Cell<Option<std::time::Instant>> = const { Cell::new(None) };
    static CTRL_START: Cell<Option<std::time::Instant>> = const { Cell::new(None) };
    static MOUSE_HANDLERS: RefCell<Vec<(HookSubscription, Box<dyn MouseHookHandler>)>> = const { RefCell::new(Vec::new()) };
    static KEYBOARD_OBSERVERS: RefCell<Vec<(HookSubscription, KeyboardObserver)>> = const { RefCell::new(Vec::new()) };
}

pub struct HookThread {
    target: HWND,
    references: usize,
    replace_win_v: bool,
    double_ctrl: bool,
    flags: Arc<AtomicU8>,
    thread: Option<(u32, JoinHandle<()>)>,
    commands: Option<mpsc::Sender<CommandRequest>>,
    mouse_subscriptions: Vec<HookSubscription>,
    keyboard_subscriptions: Vec<HookSubscription>,
    next_subscription: u64,
    _main_thread: PhantomData<Rc<()>>,
}

impl HookThread {
    pub fn new(target: HWND) -> Self {
        Self {
            target,
            references: 0,
            replace_win_v: false,
            double_ctrl: false,
            flags: Arc::new(AtomicU8::new(0)),
            thread: None,
            commands: None,
            mouse_subscriptions: Vec::new(),
            keyboard_subscriptions: Vec::new(),
            next_subscription: 0,
            _main_thread: PhantomData,
        }
    }

    pub fn references(&self) -> usize {
        self.references
            + usize::from(self.replace_win_v)
            + usize::from(self.double_ctrl)
            + self.mouse_subscriptions.len()
            + self.keyboard_subscriptions.len()
    }

    pub fn acquire(&mut self) -> Result<()> {
        self.references = self
            .references
            .checked_add(1)
            .context("Hook reference overflow")?;
        self.flags.fetch_or(MUTE, Ordering::Release);
        if let Err(error) = self.start() {
            self.references -= 1;
            if self.references == 0 {
                self.flags.fetch_and(!MUTE, Ordering::Release);
            }
            return Err(error);
        }
        Ok(())
    }

    pub fn release(&mut self) {
        self.references = self.references.saturating_sub(1);
        if self.references == 0 {
            self.flags.fetch_and(!MUTE, Ordering::Release);
        }
        if self.references() == 0 {
            self.stop();
        }
    }

    pub fn set_replace_win_v(&mut self, enabled: bool) -> Result<()> {
        if enabled == self.replace_win_v {
            return Ok(());
        }
        self.replace_win_v = enabled;
        if enabled {
            self.flags.fetch_or(WIN_V, Ordering::Release);
            if let Err(error) = self.start() {
                self.replace_win_v = false;
                self.flags.fetch_and(!WIN_V, Ordering::Release);
                return Err(error);
            }
        } else {
            self.flags.fetch_and(!WIN_V, Ordering::Release);
            if self.references() == 0 {
                self.stop();
            }
        }
        Ok(())
    }
    pub fn set_double_ctrl(&mut self, enabled: bool) -> Result<()> {
        if self.double_ctrl == enabled {
            return Ok(());
        }
        self.double_ctrl = enabled;
        if enabled {
            self.flags.fetch_or(DOUBLE_CTRL, Ordering::Release);
            if let Err(error) = self.start() {
                self.double_ctrl = false;
                self.flags.fetch_and(!DOUBLE_CTRL, Ordering::Release);
                return Err(error);
            }
        } else {
            self.flags.fetch_and(!DOUBLE_CTRL, Ordering::Release);
            if self.references() == 0 {
                self.stop();
            }
        }
        Ok(())
    }
    pub fn mouse_references(&self) -> usize {
        self.mouse_subscriptions.len()
    }

    pub fn acquire_mouse(
        &mut self,
        handler: Box<dyn MouseHookHandler>,
    ) -> Result<HookSubscription> {
        self.start()?;
        let id = self.next_id()?;
        if let Err(error) = self.command(HookCommand::AddMouse(id, handler)) {
            if self.references() == 0 {
                self.stop();
            }
            return Err(error);
        }
        self.mouse_subscriptions.push(id);
        Ok(id)
    }

    pub fn release_mouse(&mut self, id: HookSubscription) -> Result<()> {
        if self.mouse_subscriptions.contains(&id) {
            self.command(HookCommand::RemoveMouse(id))?;
            self.mouse_subscriptions.retain(|item| *item != id);
            if self.references() == 0 {
                self.stop();
            }
        }
        Ok(())
    }

    pub fn acquire_keyboard(&mut self, observer: KeyboardObserver) -> Result<HookSubscription> {
        self.start()?;
        let id = self.next_id()?;
        if let Err(error) = self.command(HookCommand::AddKeyboard(id, observer)) {
            if self.references() == 0 {
                self.stop();
            }
            return Err(error);
        }
        self.keyboard_subscriptions.push(id);
        Ok(id)
    }

    pub fn release_keyboard(&mut self, id: HookSubscription) -> Result<()> {
        if self.keyboard_subscriptions.contains(&id) {
            self.command(HookCommand::RemoveKeyboard(id))?;
            self.keyboard_subscriptions.retain(|item| *item != id);
            if self.references() == 0 {
                self.stop();
            }
        }
        Ok(())
    }

    fn next_id(&mut self) -> Result<HookSubscription> {
        self.next_subscription = self
            .next_subscription
            .checked_add(1)
            .context("Hook subscription overflow")?;
        Ok(HookSubscription(self.next_subscription))
    }

    fn command(&self, command: HookCommand) -> Result<()> {
        let (reply, result) = mpsc::sync_channel(1);
        self.commands
            .as_ref()
            .context("Hook thread is stopped")?
            .send((command, reply))
            .map_err(|_| anyhow!("Hook command channel closed"))?;
        unsafe {
            PostThreadMessageW(
                self.thread.as_ref().context("Hook thread is stopped")?.0,
                HOOK_COMMAND,
                WPARAM(0),
                LPARAM(0),
            )?;
        }
        result
            .recv()
            .context("Hook command reply closed")?
            .map_err(anyhow::Error::msg)
    }

    fn start(&mut self) -> Result<()> {
        if self.thread.is_some() {
            return Ok(());
        }
        let target = self.target.0 as usize;
        let flags = self.flags.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        let (commands, requests) = mpsc::channel::<CommandRequest>();
        let handle = thread::Builder::new()
            .name("orbment-input-hook".into())
            .spawn(move || unsafe {
                let mut message = MSG::default();
                let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
                TARGET.with(|value| value.set(target));
                FLAGS.with(|value| *value.borrow_mut() = Some(flags));
                let setup = GetModuleHandleW(None).and_then(|module| {
                    SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), Some(module.into()), 0)
                });
                let hook = match setup {
                    Ok(hook) => hook,
                    Err(error) => {
                        let _ = sender.send(Err(error.to_string()));
                        return;
                    }
                };
                let mut mouse_hook_handle = None;
                if sender.send(Ok(GetCurrentThreadId())).is_ok() {
                    loop {
                        if GetMessageW(&mut message, None, 0, 0).0 <= 0 {
                            break;
                        }
                        if message.message == HOOK_COMMAND {
                            while let Ok((command, reply)) = requests.try_recv() {
                                let result = apply_command(command, &mut mouse_hook_handle);
                                let _ = reply.send(result.map_err(|error| error.to_string()));
                            }
                            continue;
                        }
                        let _ = TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
                MOUSE_HANDLERS.with(|handlers| {
                    for (_, handler) in handlers.borrow_mut().iter_mut() {
                        handler.on_unmount();
                    }
                    handlers.borrow_mut().clear();
                });
                KEYBOARD_OBSERVERS.with(|observers| observers.borrow_mut().clear());
                if let Some(mouse) = mouse_hook_handle {
                    let _ = UnhookWindowsHookEx(mouse);
                }
                let _ = UnhookWindowsHookEx(hook);
                FLAGS.with(|value| *value.borrow_mut() = None);
            })
            .context("Starting keyboard hook thread")?;
        match receiver.recv() {
            Ok(Ok(id)) => {
                self.thread = Some((id, handle));
                self.commands = Some(commands);
                Ok(())
            }
            result => {
                let _ = handle.join();
                Err(anyhow!(match result {
                    Ok(Err(error)) => error,
                    Err(error) => error.to_string(),
                    Ok(Ok(_)) => unreachable!(),
                }))
            }
        }
    }

    fn stop(&mut self) {
        if let Some((id, handle)) = self.thread.take() {
            self.commands = None;
            unsafe {
                let _ = PostThreadMessageW(id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
            if handle.join().is_err() {
                tracing::error!("Keyboard hook thread panicked");
            }
        }
    }
}
impl Drop for HookThread {
    fn drop(&mut self) {
        self.stop();
    }
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 {
            let key = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            let recording = crate::hotkeys::is_recording();
            if recording {
                CTRL_DOWN.with(|value| value.set(false));
                CTRL_USED.with(|value| value.set(false));
                CTRL_START.with(|value| value.set(None));
                CTRL_LAST.with(|value| value.set(None));
                V_DOWN.with(|value| value.set(false));
                if key.vkCode != VK_VOLUME_MUTE.0 as u32 {
                    return CallNextHookEx(None, code, wparam, lparam);
                }
            }
            if key.vkCode != VK_VOLUME_MUTE.0 as u32
                && (key.flags.contains(LLKHF_INJECTED)
                    || key.flags.contains(LLKHF_LOWER_IL_INJECTED))
            {
                return CallNextHookEx(None, code, wparam, lparam);
            }
            let down = matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
            let mut modifiers = current_modifiers();
            match key.vkCode as u16 {
                0x11 => modifiers.ctrl = down,
                0xA2 => modifiers.ctrl = down || GetAsyncKeyState(VK_RCONTROL.0 as i32) < 0,
                0xA3 => modifiers.ctrl = down || GetAsyncKeyState(VK_LCONTROL.0 as i32) < 0,
                0x10 => modifiers.shift = down,
                0xA0 => modifiers.shift = down || GetAsyncKeyState(VK_RSHIFT.0 as i32) < 0,
                0xA1 => modifiers.shift = down || GetAsyncKeyState(VK_LSHIFT.0 as i32) < 0,
                0x12 => modifiers.alt = down,
                0xA4 => modifiers.alt = down || GetAsyncKeyState(VK_RMENU.0 as i32) < 0,
                0xA5 => modifiers.alt = down || GetAsyncKeyState(VK_LMENU.0 as i32) < 0,
                0x5B => modifiers.win = down || GetAsyncKeyState(VK_RWIN.0 as i32) < 0,
                0x5C => modifiers.win = down || GetAsyncKeyState(VK_LWIN.0 as i32) < 0,
                _ => {}
            }
            let event = KeyboardHookEvent {
                virtual_key: key.vkCode,
                scan_code: key.scanCode,
                down,
                time_ms: key.time,
                modifiers,
            };
            KEYBOARD_OBSERVERS.with(|observers| {
                for (_, observer) in observers.borrow_mut().iter_mut() {
                    observer(event);
                }
            });
            MOUSE_HANDLERS.with(|handlers| {
                for (_, handler) in handlers.borrow_mut().iter_mut() {
                    handler.on_keyboard(event);
                }
            });
            let flags = FLAGS.with(|value| {
                value
                    .borrow()
                    .as_ref()
                    .map_or(0, |value| value.load(Ordering::Acquire))
            });
            let down = matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
            let up = matches!(wparam.0 as u32, WM_KEYUP | WM_SYSKEYUP);
            let target = HWND(TARGET.with(Cell::get) as *mut _);
            if !recording && flags & DOUBLE_CTRL != 0 {
                let control = [
                    VK_CONTROL.0 as u32,
                    VK_LCONTROL.0 as u32,
                    VK_RCONTROL.0 as u32,
                ]
                .contains(&key.vkCode);
                if control {
                    if down && !CTRL_DOWN.with(|value| value.replace(true)) {
                        let modified = [VK_MENU, VK_SHIFT, VK_LWIN, VK_RWIN]
                            .into_iter()
                            .any(|key| GetAsyncKeyState(key.0 as i32) < 0);
                        CTRL_USED.with(|value| value.set(modified));
                        CTRL_START.with(|value| value.set(Some(std::time::Instant::now())));
                    }
                    if up && CTRL_DOWN.with(|value| value.replace(false)) {
                        let short = CTRL_START.with(|value| value.take()).is_some_and(|start| {
                            start.elapsed() <= std::time::Duration::from_millis(350)
                        });
                        if short && !CTRL_USED.with(Cell::get) {
                            let now = std::time::Instant::now();
                            let twice = CTRL_LAST
                                .with(|value| value.replace(Some(now)))
                                .is_some_and(|previous| {
                                    now.duration_since(previous)
                                        <= std::time::Duration::from_millis(400)
                                });
                            if twice {
                                CTRL_LAST.with(|value| value.set(None));
                                let _ = PostMessageW(
                                    Some(target),
                                    SEARCH_MESSAGE,
                                    WPARAM(0),
                                    LPARAM(0),
                                );
                            }
                        } else {
                            CTRL_LAST.with(|value| value.set(None));
                        }
                    }
                } else if down {
                    CTRL_USED.with(|value| value.set(true));
                    CTRL_LAST.with(|value| value.set(None));
                }
            } else {
                CTRL_DOWN.with(|value| value.set(false));
                CTRL_LAST.with(|value| value.set(None));
            }
            if key.vkCode == VK_VOLUME_MUTE.0 as u32 {
                if up && MUTE_DOWN.with(|value| value.replace(false)) {
                    return LRESULT(1);
                }
                if down && flags & MUTE != 0 {
                    if MUTE_DOWN.with(Cell::get) {
                        return LRESULT(1);
                    }
                    if PostMessageW(Some(target), MUTE_MESSAGE, WPARAM(0), LPARAM(0)).is_ok() {
                        MUTE_DOWN.with(|value| value.set(true));
                        return LRESULT(1);
                    }
                }
            }
            if key.vkCode == 0x56 {
                if up && V_DOWN.with(|value| value.replace(false)) {
                    return LRESULT(1);
                }
                if down && V_DOWN.with(Cell::get) {
                    return LRESULT(1);
                }
                let win = GetAsyncKeyState(VK_LWIN.0 as i32) < 0
                    || GetAsyncKeyState(VK_RWIN.0 as i32) < 0;
                let other_modifier = [VK_CONTROL, VK_MENU, VK_SHIFT]
                    .into_iter()
                    .any(|key| GetAsyncKeyState(key.0 as i32) < 0);
                if down
                    && win
                    && !other_modifier
                    && flags & WIN_V != 0
                    && PostMessageW(Some(target), CLIPBOARD_MESSAGE, WPARAM(0), LPARAM(0)).is_ok()
                {
                    V_DOWN.with(|value| value.set(true));
                    // Mark Win as used before its release so Explorer cannot open Start.
                    let events = [
                        INPUT {
                            r#type: INPUT_KEYBOARD,
                            Anonymous: INPUT_0 {
                                ki: KEYBDINPUT {
                                    wVk: VIRTUAL_KEY(0xe8),
                                    ..Default::default()
                                },
                            },
                        },
                        INPUT {
                            r#type: INPUT_KEYBOARD,
                            Anonymous: INPUT_0 {
                                ki: KEYBDINPUT {
                                    wVk: VIRTUAL_KEY(0xe8),
                                    dwFlags: KEYEVENTF_KEYUP,
                                    ..Default::default()
                                },
                            },
                        },
                    ];
                    SendInput(&events, std::mem::size_of::<INPUT>() as i32);
                    return LRESULT(1);
                }
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }
}

fn apply_command(command: HookCommand, mouse: &mut Option<HHOOK>) -> Result<()> {
    match command {
        HookCommand::AddMouse(id, handler) => {
            if mouse.is_none() {
                *mouse = Some(unsafe {
                    SetWindowsHookExW(
                        WH_MOUSE_LL,
                        Some(mouse_hook),
                        Some(GetModuleHandleW(None)?.into()),
                        0,
                    )?
                });
            }
            MOUSE_HANDLERS.with(|handlers| handlers.borrow_mut().push((id, handler)));
        }
        HookCommand::RemoveMouse(id) => {
            let last = MOUSE_HANDLERS.with(|handlers| {
                let handlers = handlers.borrow();
                handlers.len() == 1 && handlers[0].0 == id
            });
            if last && let Some(hook) = *mouse {
                unsafe { UnhookWindowsHookEx(hook)? };
                *mouse = None;
            }
            MOUSE_HANDLERS.with(|handlers| {
                let mut handlers = handlers.borrow_mut();
                if let Some(index) = handlers.iter().position(|(key, _)| *key == id) {
                    handlers.remove(index).1.on_unmount();
                }
            });
        }
        HookCommand::AddKeyboard(id, observer) => {
            KEYBOARD_OBSERVERS.with(|observers| observers.borrow_mut().push((id, observer)));
        }
        HookCommand::RemoveKeyboard(id) => {
            KEYBOARD_OBSERVERS
                .with(|observers| observers.borrow_mut().retain(|(key, _)| *key != id));
        }
    }
    Ok(())
}

fn current_modifiers() -> MouseModifiers {
    unsafe {
        MouseModifiers {
            ctrl: GetAsyncKeyState(VK_CONTROL.0 as i32) < 0,
            shift: GetAsyncKeyState(VK_SHIFT.0 as i32) < 0,
            alt: GetAsyncKeyState(VK_MENU.0 as i32) < 0,
            win: GetAsyncKeyState(VK_LWIN.0 as i32) < 0 || GetAsyncKeyState(VK_RWIN.0 as i32) < 0,
        }
    }
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 {
            let mouse = &*(lparam.0 as *const MSLLHOOKSTRUCT);
            if mouse.flags & LLMHF_INJECTED == 0 {
                let extra = if mouse.mouseData >> 16 == XBUTTON1 as u32 {
                    MouseButton::X1
                } else {
                    MouseButton::X2
                };
                let kind = match wparam.0 as u32 {
                    WM_MOUSEMOVE => Some(MouseEventKind::Move),
                    WM_LBUTTONDOWN => Some(MouseEventKind::Down(MouseButton::Left)),
                    WM_LBUTTONUP => Some(MouseEventKind::Up(MouseButton::Left)),
                    WM_RBUTTONDOWN => Some(MouseEventKind::Down(MouseButton::Right)),
                    WM_RBUTTONUP => Some(MouseEventKind::Up(MouseButton::Right)),
                    WM_MBUTTONDOWN => Some(MouseEventKind::Down(MouseButton::Middle)),
                    WM_MBUTTONUP => Some(MouseEventKind::Up(MouseButton::Middle)),
                    WM_XBUTTONDOWN => Some(MouseEventKind::Down(extra)),
                    WM_XBUTTONUP => Some(MouseEventKind::Up(extra)),
                    WM_MOUSEWHEEL | WM_MOUSEHWHEEL => Some(MouseEventKind::Wheel {
                        delta: (mouse.mouseData >> 16) as i16,
                        horizontal: wparam.0 as u32 == WM_MOUSEHWHEEL,
                    }),
                    _ => None,
                };
                if let Some(kind) = kind {
                    let event = MouseHookEvent {
                        kind,
                        x: mouse.pt.x,
                        y: mouse.pt.y,
                        time_ms: mouse.time,
                        modifiers: current_modifiers(),
                    };
                    let swallowed = MOUSE_HANDLERS.with(|handlers| {
                        let mut swallowed = false;
                        for (_, handler) in handlers.borrow_mut().iter_mut() {
                            swallowed |= handler.on_mouse(event);
                        }
                        swallowed
                    });
                    if swallowed {
                        return LRESULT(1);
                    }
                }
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Observer;
    impl MouseHookHandler for Observer {
        fn on_mouse(&mut self, _: MouseHookEvent) -> bool {
            false
        }
    }

    #[test]
    fn failed_unhook_retains_handle_and_subscription() {
        let id = HookSubscription(1);
        let hook = HHOOK::default();
        let mut mouse = Some(hook);
        MOUSE_HANDLERS.with(|handlers| handlers.borrow_mut().push((id, Box::new(Observer))));
        assert!(apply_command(HookCommand::RemoveMouse(id), &mut mouse).is_err());
        assert_eq!(mouse, Some(hook));
        MOUSE_HANDLERS.with(|handlers| {
            assert_eq!(handlers.borrow().len(), 1);
            assert_eq!(handlers.borrow()[0].0, id);
            handlers.borrow_mut().clear();
        });
    }

    #[test]
    fn failed_release_keeps_subscription_for_retry() {
        let id = HookSubscription(1);
        let mut hooks = HookThread::new(HWND::default());
        hooks.mouse_subscriptions.push(id);
        hooks.keyboard_subscriptions.push(id);
        assert!(hooks.release_mouse(id).is_err());
        assert!(hooks.release_keyboard(id).is_err());
        assert_eq!(hooks.mouse_subscriptions, [id]);
        assert_eq!(hooks.keyboard_subscriptions, [id]);
    }
}
