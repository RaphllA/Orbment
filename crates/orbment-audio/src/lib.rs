mod app_routing;
mod cycler;
mod enumerator;
pub mod knob;
pub mod osd;
mod policy_config;
mod processes;
mod volume;

use std::{
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
        mpsc::{self, Sender, SyncSender},
    },
    thread::{self, JoinHandle},
};

use anyhow::{Context, Result, anyhow, bail};
use orbment_core::{
    config::{AudioConfig, AudioDeviceSelection},
    i18n::tr,
};
use serde::Serialize;
use windows::Win32::{
    Media::Audio::{EDataFlow, eCapture, eRender},
    System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize},
};

use app_routing::AppRouting;
use enumerator::Enumerator;
use policy_config::PolicyConfig;
use processes::{ForegroundWatcher, ProcessWatcher};
use volume::MicrophoneObserver;

#[derive(Debug, Clone)]
pub struct AudioNotification {
    pub mode: String,
    pub title_key: &'static str,
    pub message: String,
}

#[derive(Debug, Clone)]
pub enum AudioEvent {
    /// Compatibility event; new hosts should render Notification instead.
    Switched(String),
    DevicesChanged,
    Error(String),
    Notification(AudioNotification),
    MicrophoneState {
        muted: bool,
        badge_mode: String,
    },
    QuickMenu {
        recording: bool,
        devices: Vec<AudioDeviceSelection>,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct AudioDeviceMetadata {
    pub id: String,
    pub icon_path: Option<String>,
    /// None means the endpoint driver did not expose its adapter topology.
    pub usb: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AudioSnapshot {
    pub playback: Vec<AudioDeviceSelection>,
    pub recording: Vec<AudioDeviceSelection>,
    #[serde(skip)]
    pub selected_playback: Vec<AudioDeviceSelection>,
    #[serde(skip)]
    pub selected_recording: Vec<AudioDeviceSelection>,
    pub playback_metadata: Vec<AudioDeviceMetadata>,
    pub recording_metadata: Vec<AudioDeviceMetadata>,
    pub default_playback: Option<String>,
    pub default_recording: Option<String>,
    pub playback_volume: Option<f32>,
    pub recording_volume: Option<f32>,
    pub microphone_muted: Option<bool>,
}

pub(crate) enum Operation {
    Switch {
        flow: EDataFlow,
        device_id: Option<String>,
        foreground: Option<u32>,
    },
    ToggleMicrophone,
    QuickMenu {
        recording: bool,
        foreground: Option<u32>,
    },
    CancelQuickMenu,
    Profile(String),
    Route {
        pid: Option<u32>,
        process_path: Option<String>,
        process_name: Option<String>,
        device_id: String,
        flow: EDataFlow,
    },
    Update(Box<AudioConfig>),
}

pub(crate) enum Command {
    Execute(Operation, SyncSender<Result<()>>),
    Snapshot(SyncSender<Result<AudioSnapshot>>),
    DevicesChanged,
    MicrophoneChanged,
    ForegroundChanged(u32),
    ProcessStarted(u32),
    WatcherFailed(String),
    Stop,
}

pub struct AudioService {
    sender: Sender<Command>,
    worker: Option<JoinHandle<()>>,
    menu_foreground: Arc<AtomicU32>,
}

/// Keeps the external foreground snapshot only for the menu and its selected action.
pub struct MenuForegroundGuard {
    foreground: Arc<AtomicU32>,
    previous: u32,
}

impl Drop for MenuForegroundGuard {
    fn drop(&mut self) {
        self.foreground.store(self.previous, Ordering::Release);
    }
}

fn foreground_target(current: Option<u32>, saved: Option<u32>) -> Option<u32> {
    let external = |pid: &u32| *pid != 0 && *pid != std::process::id();
    current.filter(external).or_else(|| saved.filter(external))
}

impl AudioService {
    /// Callbacks run on the audio worker: enqueue UI work and do not call this service from them.
    pub fn start(
        config: AudioConfig,
        notify: impl Fn(AudioEvent) + Send + 'static,
    ) -> Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let notification_sender = sender.clone();
        let worker = thread::Builder::new()
            .name("orbment-audio-mta".into())
            .spawn(move || {
                let mut state = match Worker::new(config, notification_sender, Box::new(notify)) {
                    Ok(state) => state,
                    Err(error) => {
                        let _ = ready_sender.send(Err(format!("{error:#}")));
                        return;
                    }
                };
                if ready_sender.send(Ok(())).is_err() {
                    return;
                }
                state.emit(AudioEvent::DevicesChanged);
                state.emit_microphone_state();
                if let Err(error) = state.apply_running_rules() {
                    state.report(error);
                }
                while let Ok(command) = receiver.recv() {
                    let result = match command {
                        Command::Stop => break,
                        Command::Execute(operation, reply) => {
                            // The reply carries the real operation result to CLI/MCP and hotkey callers.
                            let result = state.execute(operation);
                            let _ = reply.send(result);
                            continue;
                        }
                        Command::Snapshot(reply) => {
                            let _ = reply.send(state.snapshot());
                            continue;
                        }
                        Command::DevicesChanged => state.refresh(true),
                        Command::MicrophoneChanged => state.sync_microphone(),
                        Command::ForegroundChanged(pid) => state.process_event(pid, false),
                        Command::ProcessStarted(pid) => state.process_event(pid, true),
                        Command::WatcherFailed(detail) => {
                            state.process_watcher = None;
                            state.emit(AudioEvent::Error(detail));
                            continue;
                        }
                    };
                    if let Err(error) = result {
                        state.report(error);
                    }
                }
                state.emit(AudioEvent::MicrophoneState {
                    muted: false,
                    badge_mode: "None".into(),
                });
            })
            .with_context(|| tr("audio.worker_start_failed"))?;
        match ready_receiver.recv() {
            Ok(Ok(())) => Ok(Self {
                sender,
                worker: Some(worker),
                menu_foreground: Arc::new(AtomicU32::new(0)),
            }),
            result => {
                let detail = match result {
                    Ok(Err(detail)) => detail,
                    Err(error) => error.to_string(),
                    Ok(Ok(())) => unreachable!(),
                };
                let _ = worker.join();
                bail!("{}: {detail}", tr("audio.worker_start_failed"));
            }
        }
    }

    pub fn capture_menu_foreground(&self) -> MenuForegroundGuard {
        let foreground = foreground_target(processes::foreground_pid().ok(), None).unwrap_or(0);
        MenuForegroundGuard {
            foreground: self.menu_foreground.clone(),
            previous: self.menu_foreground.swap(foreground, Ordering::AcqRel),
        }
    }

    fn foreground(&self) -> Option<u32> {
        foreground_target(
            processes::foreground_pid().ok(),
            Some(self.menu_foreground.load(Ordering::Acquire)),
        )
    }

    /// Waits for the device switch to complete; menu requests are a separate UI action.
    pub fn switch_playback(&self) -> Result<()> {
        self.switch(eRender, None)
    }
    pub fn switch_recording(&self) -> Result<()> {
        self.switch(eCapture, None)
    }
    pub fn select_playback(&self, device_id: String) -> Result<()> {
        self.switch(eRender, Some(device_id))
    }
    pub fn select_recording(&self, device_id: String) -> Result<()> {
        self.switch(eCapture, Some(device_id))
    }
    pub fn request_quick_menu(&self, recording: bool) -> Result<()> {
        self.execute(Operation::QuickMenu {
            recording,
            foreground: self.foreground(),
        })
    }
    pub fn cancel_quick_menu(&self) -> Result<()> {
        self.execute(Operation::CancelQuickMenu)
    }
    pub fn toggle_mic_mute(&self) -> Result<()> {
        self.execute(Operation::ToggleMicrophone)
    }
    pub fn apply_profile(&self, id: String) -> Result<()> {
        self.execute(Operation::Profile(id))
    }
    pub fn update_config(&self, config: AudioConfig) -> Result<()> {
        self.execute(Operation::Update(Box::new(config)))
    }

    pub fn route_process(
        &self,
        pid: Option<u32>,
        process_path: Option<String>,
        process_name: Option<String>,
        device_id: String,
        recording: bool,
    ) -> Result<()> {
        let pid = if pid.is_none() && process_path.is_none() && process_name.is_none() {
            Some(processes::foreground_pid()?)
        } else {
            pid
        };
        self.execute(Operation::Route {
            pid,
            process_path,
            process_name,
            device_id,
            flow: if recording { eCapture } else { eRender },
        })
    }

    pub fn snapshot(&self) -> Result<AudioSnapshot> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.send(Command::Snapshot(reply))?;
        receiver
            .recv()
            .map_err(|_| anyhow!(tr("audio.worker_stopped")))?
    }

    fn switch(&self, flow: EDataFlow, device_id: Option<String>) -> Result<()> {
        self.execute(Operation::Switch {
            flow,
            device_id,
            foreground: self.foreground(),
        })
    }

    fn execute(&self, operation: Operation) -> Result<()> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.send(Command::Execute(operation, reply))?;
        receiver
            .recv()
            .map_err(|_| anyhow!(tr("audio.worker_stopped")))?
    }

    fn send(&self, command: Command) -> Result<()> {
        self.sender
            .send(command)
            .map_err(|_| anyhow!(tr("audio.worker_stopped")))
    }
}

impl Drop for AudioService {
    fn drop(&mut self) {
        let _ = self.sender.send(Command::Stop);
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            tracing::error!("Audio worker panicked");
        }
    }
}

struct Apartment;
impl Apartment {
    fn new() -> Result<Self> {
        unsafe { RoInitialize(RO_INIT_MULTITHREADED) }
            .context("RoInitialize(RO_INIT_MULTITHREADED)")?;
        Ok(Self)
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { RoUninitialize() };
    }
}

struct Worker {
    microphone: Option<MicrophoneObserver>,
    process_watcher: Option<ProcessWatcher>,
    foreground_watcher: Option<ForegroundWatcher>,
    enumerator: Enumerator,
    policy: PolicyConfig,
    routing: Option<AppRouting>,
    config: AudioConfig,
    playback: Vec<AudioDeviceSelection>,
    recording: Vec<AudioDeviceSelection>,
    microphone_muted: Option<bool>,
    menu_foreground: Option<u32>,
    sender: Sender<Command>,
    notify: Box<dyn Fn(AudioEvent) + Send>,
    // COM interfaces and callbacks must be released before the apartment.
    _apartment: Apartment,
}

impl Worker {
    fn new(
        mut config: AudioConfig,
        sender: Sender<Command>,
        notify: Box<dyn Fn(AudioEvent) + Send>,
    ) -> Result<Self> {
        let apartment = Apartment::new()?;
        let enumerator = Enumerator::new(sender.clone())?;
        let policy = PolicyConfig::new()?;
        let playback = enumerator.endpoints(eRender)?;
        let recording = enumerator.endpoints(eCapture)?;
        playback.reconcile(&mut config.selected_playback_devices);
        recording.reconcile(&mut config.selected_recording_devices);
        let mut state = Self {
            microphone: None,
            process_watcher: None,
            foreground_watcher: None,
            enumerator,
            policy,
            routing: None,
            config,
            playback: playback.connected,
            recording: recording.connected,
            microphone_muted: None,
            menu_foreground: None,
            sender,
            notify,
            _apartment: apartment,
        };
        state.configure_watchers();
        state.sync_microphone()?;
        Ok(state)
    }

    fn emit(&self, event: AudioEvent) {
        (self.notify)(event);
    }
    fn report(&self, error: anyhow::Error) {
        self.emit(AudioEvent::Error(format!(
            "{}: {error:#}",
            tr("audio.operation_failed")
        )));
    }
    fn notification(&self, mode: &str, title_key: &'static str, message: String) {
        self.emit(AudioEvent::Notification(AudioNotification {
            mode: mode.into(),
            title_key,
            message,
        }));
    }

    fn configure_watchers(&mut self) {
        let process_needed = self.config.app_sound_rules.iter().any(|rule| rule.enabled)
            || self.config.profiles.iter().any(|profile| {
                profile.enabled && profile.on_process_start && profile.process_path.is_some()
            });
        let foreground_needed = self.config.profiles.iter().any(|profile| {
            profile.enabled && profile.on_window_activate && profile.process_path.is_some()
        });
        if process_needed && self.process_watcher.is_none() {
            match ProcessWatcher::new(self.sender.clone()) {
                Ok(watcher) => self.process_watcher = Some(watcher),
                Err(error) => self.report(error.context(tr("audio.process_watch_failed"))),
            }
        } else if !process_needed {
            self.process_watcher = None;
        }
        if foreground_needed && self.foreground_watcher.is_none() {
            match ForegroundWatcher::new(self.sender.clone()) {
                Ok(watcher) => self.foreground_watcher = Some(watcher),
                Err(error) => self.report(error),
            }
        } else if !foreground_needed {
            self.foreground_watcher = None;
        }
    }

    fn execute(&mut self, operation: Operation) -> Result<()> {
        match operation {
            Operation::Switch {
                flow,
                device_id,
                foreground,
            } => self.switch_device(flow, device_id, foreground),
            Operation::QuickMenu {
                recording,
                foreground,
            } => {
                self.menu_foreground = None;
                self.refresh(false)?;
                self.menu_foreground = foreground_target(foreground, None);
                let devices = if recording {
                    &self.recording
                } else {
                    &self.playback
                };
                self.emit(AudioEvent::QuickMenu {
                    recording,
                    devices: devices.clone(),
                });
                Ok(())
            }
            Operation::CancelQuickMenu => {
                self.menu_foreground = None;
                Ok(())
            }
            Operation::ToggleMicrophone => {
                self.sync_microphone()?;
                self.microphone
                    .as_ref()
                    .ok_or_else(|| anyhow!(tr("audio.no_recording_device")))?
                    .toggle()?;
                self.sync_microphone()
            }
            Operation::Profile(id) => self.apply_profile(&id),
            Operation::Route {
                pid,
                process_path,
                process_name,
                device_id,
                flow,
            } => {
                self.find_device(flow, &device_id)?;
                let pids = if let Some(pid) = pid {
                    vec![pid]
                } else {
                    processes::running_processes()?
                        .into_iter()
                        .filter_map(|(pid, path)| {
                            let matches = process_path.as_ref().map_or_else(
                                || {
                                    process_name
                                        .as_ref()
                                        .is_some_and(|name| processes::same_name(&path, name))
                                },
                                |expected| processes::same_path(&path, expected),
                            );
                            matches.then_some(pid)
                        })
                        .collect()
                };
                if pids.is_empty() {
                    bail!(tr("audio.process_not_found"));
                }
                for pid in pids {
                    self.route(pid, flow, &device_id, self.config.change_communications)?;
                }
                Ok(())
            }
            Operation::Update(config) => {
                self.config = *config;
                self.menu_foreground = None;
                self.configure_watchers();
                self.refresh(false)?;
                self.emit_microphone_state();
                self.apply_running_rules()
            }
        }
    }

    fn refresh(&mut self, notify: bool) -> Result<()> {
        let playback = self.enumerator.endpoints(eRender)?;
        let recording = self.enumerator.endpoints(eCapture)?;
        let mut changed = playback.reconcile(&mut self.config.selected_playback_devices);
        changed |= recording.reconcile(&mut self.config.selected_recording_devices);
        let arrived = playback
            .connected
            .iter()
            .any(|device| !self.playback.iter().any(|old| old.id == device.id))
            || recording
                .connected
                .iter()
                .any(|device| !self.recording.iter().any(|old| old.id == device.id));
        if self.config.auto_add_new_devices {
            changed |= add_new_devices(
                &self.playback,
                &playback.connected,
                &mut self.config.selected_playback_devices,
            );
            changed |= add_new_devices(
                &self.recording,
                &recording.connected,
                &mut self.config.selected_recording_devices,
            );
        }
        self.playback = playback.connected;
        self.recording = recording.connected;
        if notify || changed {
            self.emit(AudioEvent::DevicesChanged);
        }
        self.sync_microphone()?;
        if arrived && let Err(error) = self.apply_running_rules() {
            self.report(error);
        }
        Ok(())
    }

    fn sync_microphone(&mut self) -> Result<()> {
        let id = self.enumerator.default_id(eCapture)?;
        let changed_device = self.microphone.as_ref().map(|observer| &observer.id) != id.as_ref();
        if changed_device {
            self.microphone = None;
            if let Some(id) = id {
                self.microphone = Some(MicrophoneObserver::new(
                    id.clone(),
                    self.enumerator.volume(&id)?,
                    self.sender.clone(),
                )?);
            }
        }
        let muted = self
            .microphone
            .as_ref()
            .map(MicrophoneObserver::muted)
            .transpose()?;
        let changed_mute = self.microphone_muted != muted;
        if changed_mute && let (Some(_), Some(muted)) = (self.microphone_muted, muted) {
            let key = if muted {
                "audio.microphone_muted"
            } else {
                "audio.microphone_unmuted"
            };
            let name = self
                .recording
                .iter()
                .find(|device| {
                    self.microphone
                        .as_ref()
                        .is_some_and(|observer| observer.id == device.id)
                })
                .map(|device| device.name.clone())
                .unwrap_or_default();
            self.notification(&self.config.mic_mute_notification, key, name);
        }
        self.microphone_muted = muted;
        if changed_device || changed_mute {
            self.emit_microphone_state();
        }
        Ok(())
    }

    fn emit_microphone_state(&self) {
        let mode = match self.microphone_muted {
            Some(true) => self.config.mic_mute_banner_mode.as_str(),
            Some(false) => self.config.mic_unmute_banner_mode.as_str(),
            None => "None",
        };
        self.emit(AudioEvent::MicrophoneState {
            muted: self.microphone_muted.unwrap_or(false),
            badge_mode: mode.into(),
        });
    }

    fn switch_device(
        &mut self,
        flow: EDataFlow,
        id: Option<String>,
        foreground: Option<u32>,
    ) -> Result<()> {
        let menu_foreground = self.menu_foreground.take();
        self.refresh(false)?;
        let devices = if flow == eRender {
            &self.playback
        } else {
            &self.recording
        };
        let next = if let Some(id) = id {
            devices
                .iter()
                .find(|device| device.id == id)
                .cloned()
                .ok_or_else(|| anyhow!("{}: {id}", tr("audio.device_unavailable")))?
        } else {
            let selected = if flow == eRender {
                &self.config.selected_playback_devices
            } else {
                &self.config.selected_recording_devices
            };
            let current = self.enumerator.default_id(flow)?;
            cycler::next_device(
                devices,
                selected,
                &self.config.cycle_mode,
                current.as_deref(),
            )?
            .clone()
        };
        let foreground = if self.config.switch_foreground_program {
            foreground_target(menu_foreground, foreground)
        } else {
            None
        };
        self.set_endpoint(flow, &next.id, self.config.change_communications)?;
        if let Some(pid) = foreground {
            self.route(pid, flow, &next.id, self.config.change_communications)?;
        }
        self.sync_microphone()?;
        self.emit(AudioEvent::Switched(next.name.clone()));
        let key = if flow == eRender {
            "audio.playback_switched"
        } else {
            "audio.recording_switched"
        };
        self.notification(&self.config.switch_device_notification, key, next.name);
        Ok(())
    }

    fn find_device(&self, flow: EDataFlow, id: &str) -> Result<AudioDeviceSelection> {
        self.enumerator
            .endpoints(flow)?
            .connected
            .into_iter()
            .find(|device| device.id == id)
            .ok_or_else(|| anyhow!("{}: {id}", tr("audio.device_unavailable")))
    }

    fn set_endpoint(&self, flow: EDataFlow, id: &str, communications: bool) -> Result<()> {
        let volume = if self.config.keep_volume_enabled {
            self.enumerator
                .default_id(flow)?
                .map(|old| -> Result<f32> {
                    Ok(unsafe { self.enumerator.volume(&old)?.GetMasterVolumeLevelScalar()? })
                })
                .transpose()?
        } else {
            None
        };
        if let Some(volume) = volume {
            // Apply before changing defaults so playback never starts at the target's old volume.
            unsafe {
                self.enumerator
                    .volume(id)?
                    .SetMasterVolumeLevelScalar(volume, std::ptr::null())?
            };
        }
        self.policy.set_default(id, communications)
    }

    fn route(&mut self, pid: u32, flow: EDataFlow, id: &str, communications: bool) -> Result<()> {
        if self.routing.is_none() {
            self.routing =
                Some(AppRouting::new().with_context(|| tr("audio.routing_unavailable"))?);
        }
        self.routing
            .as_ref()
            .expect("routing initialized")
            .route(pid, flow, id, communications)
            .with_context(|| tr("audio.routing_failed"))
    }

    fn apply_profile(&mut self, id: &str) -> Result<()> {
        let profile = self
            .config
            .profiles
            .iter()
            .find(|profile| profile.id == id && profile.enabled)
            .cloned()
            .ok_or_else(|| anyhow!("{}: {id}", tr("audio.profile_not_found")))?;
        // Check both endpoints before changing either; role updates themselves are sequential Win32 calls.
        if let Some(id) = &profile.playback_device_id {
            self.find_device(eRender, id)?;
        }
        if let Some(id) = &profile.recording_device_id {
            self.find_device(eCapture, id)?;
        }
        if let Some(id) = &profile.playback_device_id {
            self.set_endpoint(eRender, id, profile.change_communications)?;
        }
        if let Some(id) = &profile.recording_device_id {
            self.set_endpoint(eCapture, id, profile.change_communications)?;
        }
        self.sync_microphone()?;
        self.notification(
            &self.config.switch_profile_notification,
            "audio.profile_switched",
            profile.name,
        );
        Ok(())
    }

    fn apply_running_rules(&mut self) -> Result<()> {
        if self.config.app_sound_rules.iter().any(|rule| rule.enabled) {
            for (pid, path) in processes::running_processes()? {
                self.apply_rules(pid, &path)?;
            }
        }
        Ok(())
    }

    fn apply_rules(&mut self, pid: u32, path: &str) -> Result<()> {
        let rules: Vec<_> = self
            .config
            .app_sound_rules
            .iter()
            .filter(|rule| rule.enabled && processes::same_path(&rule.process_path, path))
            .cloned()
            .collect();
        for rule in rules {
            for (flow, id) in [
                (eRender, rule.playback_device_id),
                (eCapture, rule.recording_device_id),
            ] {
                if let Some(id) = id {
                    let devices = if flow == eRender {
                        &self.playback
                    } else {
                        &self.recording
                    };
                    // Offline rule targets remain configured and are retried on endpoint arrival.
                    if devices.iter().any(|device| device.id == id) {
                        self.route(pid, flow, &id, self.config.change_communications)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn process_event(&mut self, pid: u32, started: bool) -> Result<()> {
        // A process may exit before its queued event is handled, or deny image-path access.
        let Ok(path) = processes::process_path(pid) else {
            return Ok(());
        };
        if started {
            self.apply_rules(pid, &path)?;
        }
        let profile = self
            .config
            .profiles
            .iter()
            .find(|profile| {
                profile.enabled
                    && (if started {
                        profile.on_process_start
                    } else {
                        profile.on_window_activate
                    })
                    && profile
                        .process_path
                        .as_ref()
                        .is_some_and(|expected| processes::same_path(expected, &path))
            })
            .map(|profile| profile.id.clone());
        if let Some(id) = profile {
            self.apply_profile(&id)?;
        }
        Ok(())
    }

    fn snapshot(&self) -> Result<AudioSnapshot> {
        let playback = self.playback.clone();
        let recording = self.recording.clone();
        let default_playback = self.enumerator.default_id(eRender)?;
        let default_recording = self.enumerator.default_id(eCapture)?;
        let scalar = |id: &Option<String>| -> Result<Option<f32>> {
            id.as_ref()
                .map(|id| -> Result<f32> {
                    Ok(unsafe { self.enumerator.volume(id)?.GetMasterVolumeLevelScalar()? })
                })
                .transpose()
        };
        let microphone_muted = default_recording
            .as_ref()
            .map(|id| -> Result<bool> {
                Ok(unsafe { self.enumerator.volume(id)?.GetMute()? }.as_bool())
            })
            .transpose()?;
        Ok(AudioSnapshot {
            playback_metadata: self.enumerator.metadata(&playback),
            recording_metadata: self.enumerator.metadata(&recording),
            playback_volume: scalar(&default_playback)?,
            recording_volume: scalar(&default_recording)?,
            playback,
            recording,
            selected_playback: self.config.selected_playback_devices.clone(),
            selected_recording: self.config.selected_recording_devices.clone(),
            default_playback,
            default_recording,
            microphone_muted,
        })
    }
}

fn add_new_devices(
    previous: &[AudioDeviceSelection],
    connected: &[AudioDeviceSelection],
    selected: &mut Vec<AudioDeviceSelection>,
) -> bool {
    let mut added = false;
    for device in connected {
        if !previous.iter().any(|old| old.id == device.id)
            && !selected.iter().any(|chosen| chosen.id == device.id)
        {
            selected.push(device.clone());
            added = true;
        }
    }
    added
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreground_routing_never_targets_our_own_process() {
        let own = std::process::id();
        let external = own.wrapping_add(1).max(1);
        let other = own.wrapping_add(2).max(1);
        assert_eq!(foreground_target(Some(own), Some(external)), Some(external));
        assert_eq!(foreground_target(Some(other), Some(external)), Some(other));
        assert_eq!(foreground_target(None, Some(external)), Some(external));
        assert_eq!(foreground_target(Some(0), Some(external)), Some(external));
        assert_eq!(foreground_target(Some(own), None), None);
        assert_eq!(foreground_target(None, Some(own)), None);
        assert_eq!(foreground_target(Some(0), Some(0)), None);
    }

    #[test]
    fn menu_foreground_is_cleared_when_the_selected_action_finishes() {
        let foreground = Arc::new(AtomicU32::new(42));
        {
            let _guard = MenuForegroundGuard {
                foreground: foreground.clone(),
                previous: 0,
            };
            assert_eq!(foreground.load(Ordering::Acquire), 42);
        }
        assert_eq!(foreground.load(Ordering::Acquire), 0);
    }

    #[test]
    fn auto_add_only_adds_arrivals_and_preserves_disconnected_selections() {
        let device = |id: &str| AudioDeviceSelection {
            id: id.into(),
            name: id.into(),
        };
        let mut selected = vec![device("unplugged")];
        assert!(add_new_devices(
            &[device("existing")],
            &[device("existing"), device("new")],
            &mut selected
        ));
        assert_eq!(
            selected
                .iter()
                .map(|device| device.id.as_str())
                .collect::<Vec<_>>(),
            ["unplugged", "new"]
        );
        assert!(!add_new_devices(&[], &[device("new")], &mut selected));
    }
}
