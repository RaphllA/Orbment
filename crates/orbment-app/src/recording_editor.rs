use anyhow::{Result, bail, ensure};
use orbment_core::i18n::tr;
use orbment_ui::{D2dContext, PopupPanel, SettingsEvent, TransientPanelWindow};
use orbment_vision::{
    CapturedImage,
    recorder::{EffectOptions, ExportFormat, RecordedSession, TICKS_PER_SECOND, TrimRange},
};
use std::{
    collections::VecDeque,
    mem::size_of,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::HWND,
        Media::{Audio::*, MMTIME, TIME_BYTES, TIME_MS, TIME_SAMPLES},
    },
    core::PSTR,
};

struct EditorState {
    duration: i64,
    position: i64,
    trim: TrimRange,
    effects: EffectOptions,
    format: ExportFormat,
    path: String,
    audio: bool,
    playing: bool,
    exporting: bool,
    exported: bool,
    preview: Option<egui::ColorImage>,
    error: Option<String>,
    media_warning: Option<String>,
    close_requested: bool,
}

#[derive(Clone)]
struct Playback {
    position: i64,
    trim: TrimRange,
    effects: EffectOptions,
    audio: bool,
    playing: bool,
}
impl EditorState {
    fn playback(&self) -> Playback {
        Playback {
            position: self.position,
            trim: self.trim,
            effects: self.effects.clone(),
            audio: self.audio,
            playing: self.playing,
        }
    }
}
enum Command {
    Update(Playback),
    Export {
        path: PathBuf,
        format: ExportFormat,
        trim: TrimRange,
        effects: EffectOptions,
    },
    Cancel,
}
enum WorkerEvent {
    Exported(PathBuf),
    Closed,
}

pub struct RecordingEditor {
    popup: TransientPanelWindow,
    state: Arc<Mutex<EditorState>>,
    sender: Sender<Command>,
    events: Receiver<WorkerEvent>,
    cancel: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    close_after_export: bool,
    closed: bool,
    was_visible: bool,
    device_lost: bool,
}
impl RecordingEditor {
    pub fn new(
        graphics: Rc<D2dContext>,
        hwnd: HWND,
        recording: RecordedSession,
        path: PathBuf,
        format: ExportFormat,
        effects: EffectOptions,
        close_after_export: bool,
    ) -> Result<Self> {
        let duration = recording.duration_100ns();
        ensure!(duration > 0, "Recording contains no playable frames");
        let state = Arc::new(Mutex::new(EditorState {
            duration,
            position: 0,
            trim: recording.full_range(),
            effects,
            format,
            path: path.to_string_lossy().into_owned(),
            audio: true,
            playing: false,
            exporting: false,
            exported: false,
            preview: None,
            error: None,
            media_warning: recording.media_unavailable().map(str::to_owned),
            close_requested: false,
        }));
        let (sender, commands) = mpsc::channel();
        let (updates, events) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let panel = EditorPanel {
            state: state.clone(),
            sender: sender.clone(),
            cancel: cancel.clone(),
            texture: None,
            wake: hwnd.0 as usize,
        };
        let popup = TransientPanelWindow::new(graphics, hwnd, Box::new(panel), Instant::now());
        popup.set_dismiss_on_blur(false);
        popup.show()?;
        let worker_state = state.clone();
        let worker_cancel = cancel.clone();
        let wake = hwnd.0 as usize;
        let worker = thread::Builder::new()
            .name("orbment-recording-editor".into())
            .spawn(move || {
                playback_worker(
                    recording,
                    commands,
                    updates,
                    worker_state,
                    worker_cancel,
                    wake,
                );
            })?;
        Ok(Self {
            popup,
            state,
            sender,
            events,
            cancel,
            worker: Some(worker),
            close_after_export,
            closed: false,
            was_visible: true,
            device_lost: false,
        })
    }
    pub fn show(&mut self) -> Result<()> {
        self.popup.show()?;
        self.was_visible = true;
        Ok(())
    }
    pub fn hide(&mut self) {
        self.popup.hide();
    }
    pub fn take_device_lost(&mut self) -> bool {
        std::mem::take(&mut self.device_lost)
    }
    pub fn is_visible(&self) -> bool {
        self.popup.is_visible()
    }
    pub fn is_closed(&self) -> bool {
        self.closed
    }
    pub fn rebuild_graphics(&self, graphics: Rc<D2dContext>) -> Result<()> {
        self.popup.rebuild_graphics(graphics)
    }
    pub fn tick(&mut self) -> Result<Vec<PathBuf>> {
        for event in self.popup.take_events() {
            match event {
                SettingsEvent::Error(error) => bail!("{error}"),
                SettingsEvent::DeviceLost => self.device_lost = true,
                _ => {}
            }
        }
        let mut exported = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            match event {
                WorkerEvent::Exported(path) => {
                    exported.push(path);
                    if self.close_after_export {
                        self.closed = true;
                        self.popup.unload();
                    }
                }
                WorkerEvent::Closed => {
                    self.closed = true;
                    self.popup.unload();
                }
            }
        }
        let close_requested = self
            .state
            .lock()
            .expect("recording editor lock poisoned")
            .close_requested;
        if close_requested && !self.closed {
            self.cancel();
        }
        let visible = self.popup.is_visible();
        if self.was_visible && !visible && !self.closed {
            let mut state = self.state.lock().expect("recording editor lock poisoned");
            state.playing = false;
            if !state.exported && !state.exporting {
                let _ = self.sender.send(Command::Update(state.playback()));
            }
        }
        self.was_visible = visible;
        if visible {
            self.popup.request_repaint();
        }
        if self
            .worker
            .as_ref()
            .is_some_and(|worker| worker.is_finished())
        {
            self.worker
                .take()
                .expect("finished worker")
                .join()
                .map_err(|_| anyhow::anyhow!("Recording editor worker panicked"))?;
        }
        Ok(exported)
    }
    pub fn cancel(&mut self) {
        self.cancel.store(true, Ordering::Release);
        let _ = self.sender.send(Command::Cancel);
        self.popup.unload();
        self.closed = true;
    }
    pub fn shutdown(&mut self) -> Result<()> {
        self.cancel();
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("Recording editor worker panicked"))?;
        }
        Ok(())
    }
}
impl Drop for RecordingEditor {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            tracing::warn!(%error, "Recording editor shutdown");
        }
    }
}

struct EditorPanel {
    state: Arc<Mutex<EditorState>>,
    sender: Sender<Command>,
    cancel: Arc<AtomicBool>,
    texture: Option<egui::TextureHandle>,
    wake: usize,
}
impl PopupPanel for EditorPanel {
    fn title(&self) -> String {
        tr("recording_editor.title")
    }
    fn ui(&mut self, context: &egui::Context) {
        let mut state = self.state.lock().expect("recording editor lock poisoned");
        if let Some(image) = state.preview.take() {
            if let Some(texture) = &mut self.texture {
                texture.set(image, egui::TextureOptions::LINEAR);
            } else {
                self.texture = Some(context.load_texture(
                    "recording-preview",
                    image,
                    egui::TextureOptions::LINEAR,
                ));
            }
        }
        let mut changed = false;
        let mut export = false;
        let mut close = false;
        egui::CentralPanel::default().show(context, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                if let Some(texture) = &self.texture {
                    let size = texture.size_vec2();
                    let scale = (ui.available_width() / size.x).min(280.0 / size.y).min(1.0);
                    ui.image((texture.id(), size * scale));
                } else {
                    ui.label(tr("recording_editor.loading"));
                }
                if let Some(warning) = &state.media_warning {
                    ui.colored_label(egui::Color32::YELLOW, warning);
                }
                if let Some(error) = &state.error {
                    ui.colored_label(egui::Color32::RED, error);
                }
                let editable = !state.exporting && !state.exported;
                ui.add_enabled_ui(editable, |ui| {
                    ui.horizontal(|ui| {
                        if ui
                            .button(tr(if state.playing {
                                "recording_editor.pause"
                            } else {
                                "recording_editor.play"
                            }))
                            .clicked()
                        {
                            if !state.playing
                                && (state.position < state.trim.start_100ns
                                    || state.position >= state.trim.end_100ns - 1)
                            {
                                state.position = state.trim.start_100ns;
                            }
                            state.playing = !state.playing;
                            changed = true;
                        }
                        changed |= ui
                            .checkbox(&mut state.audio, tr("recording_editor.audio"))
                            .changed();
                        ui.label(format!(
                            "{:.2} / {:.2} s",
                            seconds(state.position),
                            seconds(state.duration)
                        ));
                    });
                    let mut position = seconds(state.position);
                    if ui
                        .add(
                            egui::Slider::new(&mut position, 0.0..=seconds(state.duration))
                                .text(tr("recording_editor.seek")),
                        )
                        .changed()
                    {
                        state.position = ticks(position).min(state.duration - 1);
                        changed = true;
                    }
                    let mut start = seconds(state.trim.start_100ns);
                    let mut end = seconds(state.trim.end_100ns);
                    let trim_changed = ui
                        .add(
                            egui::Slider::new(&mut start, 0.0..=seconds(state.duration))
                                .text(tr("recording_editor.trim_start")),
                        )
                        .changed()
                        | ui.add(
                            egui::Slider::new(&mut end, 0.0..=seconds(state.duration))
                                .text(tr("recording_editor.trim_end")),
                        )
                        .changed();
                    if trim_changed {
                        state.trim.start_100ns = ticks(start).min(state.duration - 1);
                        state.trim.end_100ns =
                            ticks(end).clamp(state.trim.start_100ns + 1, state.duration);
                        state.position = state
                            .position
                            .clamp(state.trim.start_100ns, state.trim.end_100ns - 1);
                        changed = true;
                    }
                    ui.collapsing(tr("recording_editor.effects"), |ui| {
                        let effects = &mut state.effects;
                        changed |= ui
                            .checkbox(&mut effects.show_pointer, tr("recording_editor.pointer"))
                            .changed();
                        changed |= ui
                            .checkbox(
                                &mut effects.smooth_pointer,
                                tr("recording_editor.smooth_track"),
                            )
                            .changed();
                        changed |= ui
                            .checkbox(
                                &mut effects.simple_pointer,
                                tr("recording_editor.simple_animation"),
                            )
                            .changed();
                        changed |= ui
                            .add(
                                egui::Slider::new(&mut effects.pointer_scale, 0.1..=4.0)
                                    .text(tr("recording_editor.pointer_scale")),
                            )
                            .changed();
                        changed |= ui
                            .add(
                                egui::Slider::new(&mut effects.animation_speed, 0.1..=4.0)
                                    .text(tr("recording_editor.animation_speed")),
                            )
                            .changed();
                        for (label, color) in [
                            ("recording_editor.left_color", &mut effects.left_color),
                            ("recording_editor.right_color", &mut effects.right_color),
                            ("recording_editor.middle_color", &mut effects.middle_color),
                        ] {
                            ui.horizontal(|ui| {
                                ui.label(tr(label));
                                changed |= ui.color_edit_button_srgba_unmultiplied(color).changed();
                            });
                        }
                        changed |= ui
                            .checkbox(
                                &mut effects.show_characters,
                                tr("recording_editor.characters"),
                            )
                            .changed();
                        changed |= ui
                            .checkbox(
                                &mut effects.show_combinations,
                                tr("recording_editor.combinations"),
                            )
                            .changed();
                        changed |= ui
                            .checkbox(
                                &mut effects.show_modifiers,
                                tr("recording_editor.modifiers"),
                            )
                            .changed();
                        changed |= ui
                            .checkbox(
                                &mut effects.show_function_keys,
                                tr("recording_editor.function_keys"),
                            )
                            .changed();
                        let mut keep_ms = effects.key_stay_100ns / 10_000;
                        if ui
                            .add(
                                egui::Slider::new(&mut keep_ms, 100..=10_000)
                                    .text(tr("recording_editor.key_keep_time")),
                            )
                            .changed()
                        {
                            effects.key_stay_100ns = keep_ms * 10_000;
                            changed = true;
                        }
                        changed |= ui
                            .add(
                                egui::Slider::new(&mut effects.key_history_rows, 0..=32)
                                    .text(tr("recording_editor.max_history")),
                            )
                            .changed();
                        changed |= ui
                            .add(
                                egui::Slider::new(&mut effects.character_merge_limit, 0..=256)
                                    .text(tr("recording_editor.merge_limit")),
                            )
                            .changed();
                        changed |= ui
                            .add(
                                egui::Slider::new(&mut effects.key_font_size, 8..=128)
                                    .text(tr("recording_editor.font_size")),
                            )
                            .changed();
                        ui.horizontal(|ui| {
                            ui.label(tr("recording_editor.text_color"));
                            changed |= ui
                                .color_edit_button_srgba_unmultiplied(&mut effects.key_text_color)
                                .changed();
                        });
                        ui.horizontal(|ui| {
                            ui.label(tr("recording_editor.background_color"));
                            changed |= ui
                                .color_edit_button_srgba_unmultiplied(
                                    &mut effects.key_background_color,
                                )
                                .changed();
                        });
                        changed |= ui
                            .add(
                                egui::Slider::new(&mut effects.system_audio_gain, 0.0..=2.0)
                                    .text(tr("recording_editor.system_gain")),
                            )
                            .changed();
                        changed |= ui
                            .add(
                                egui::Slider::new(&mut effects.microphone_gain, 0.0..=2.0)
                                    .text(tr("recording_editor.microphone_gain")),
                            )
                            .changed();
                    });
                    let previous_format = state.format;
                    egui::ComboBox::from_id_salt("recording-format")
                        .icon(orbment_ui::theme::combo_icon)
                        .selected_text(format_name(state.format))
                        .show_ui(ui, |ui| {
                            for format in [ExportFormat::Mp4, ExportFormat::Gif, ExportFormat::WebP]
                            {
                                ui.selectable_value(&mut state.format, format, format_name(format));
                            }
                        });
                    if previous_format != state.format {
                        state.path = PathBuf::from(&state.path)
                            .with_extension(extension(state.format))
                            .to_string_lossy()
                            .into_owned();
                    }
                    ui.label(tr("recording_editor.output_path"));
                    ui.text_edit_singleline(&mut state.path);
                    export = ui.button(tr("recording_editor.export")).clicked();
                });
                if state.exporting {
                    ui.spinner();
                    ui.label(tr("recording_editor.exporting"));
                }
                if state.exported {
                    ui.label(tr("recording_editor.exported"));
                }
                close = ui
                    .button(tr(if state.exported {
                        "recording_editor.close"
                    } else {
                        "recording_editor.cancel"
                    }))
                    .clicked();
            });
        });
        if changed {
            state.error = None;
            if self.sender.send(Command::Update(state.playback())).is_err() {
                state.error = Some(tr("recording_editor.worker_stopped"));
            }
        }
        if export {
            if state.path.trim().is_empty() {
                state.error = Some(tr("recording_editor.path_required"));
            } else {
                state.playing = false;
                state.exporting = true;
                state.error = None;
                if self
                    .sender
                    .send(Command::Export {
                        path: PathBuf::from(&state.path),
                        format: state.format,
                        trim: state.trim,
                        effects: state.effects.clone(),
                    })
                    .is_err()
                {
                    state.exporting = false;
                    state.error = Some(tr("recording_editor.worker_stopped"));
                }
            }
        }
        if close {
            state.close_requested = true;
            self.cancel.store(true, Ordering::Release);
            let _ = self.sender.send(Command::Cancel);
            orbment_ui::wake(HWND(self.wake as *mut _));
        }
    }
}

fn seconds(value: i64) -> f64 {
    value as f64 / TICKS_PER_SECOND as f64
}
fn ticks(value: f64) -> i64 {
    (value * TICKS_PER_SECOND as f64).round() as i64
}
fn format_name(format: ExportFormat) -> &'static str {
    match format {
        ExportFormat::Mp4 => "MP4",
        ExportFormat::Gif => "GIF",
        ExportFormat::WebP => "WebP",
    }
}
fn extension(format: ExportFormat) -> &'static str {
    match format {
        ExportFormat::Mp4 => "mp4",
        ExportFormat::Gif => "gif",
        ExportFormat::WebP => "webp",
    }
}

fn preview_image(frame: CapturedImage) -> Result<egui::ColorImage> {
    frame.validate()?;
    let scale = (960.0 / f64::from(frame.width))
        .min(540.0 / f64::from(frame.height))
        .min(1.0);
    let width = (f64::from(frame.width) * scale).round().max(1.0) as usize;
    let height = (f64::from(frame.height) * scale).round().max(1.0) as usize;
    let mut image = egui::ColorImage::new([width, height], egui::Color32::TRANSPARENT);
    // Sample and convert only preview pixels; no full-frame clone or conversion reaches the UI.
    for y in 0..height {
        let source_y = y * frame.height as usize / height;
        for x in 0..width {
            let source_x = x * frame.width as usize / width;
            let offset = (source_y * frame.width as usize + source_x) * 4;
            let bgra = &frame.pixels[offset..offset + 4];
            image.pixels[y * width + x] =
                egui::Color32::from_rgba_unmultiplied(bgra[2], bgra[1], bgra[0], bgra[3]);
        }
    }
    Ok(image)
}

fn playback_worker(
    mut recording: RecordedSession,
    commands: Receiver<Command>,
    events: Sender<WorkerEvent>,
    state: Arc<Mutex<EditorState>>,
    cancel: Arc<AtomicBool>,
    wake: usize,
) {
    let mut playback = state
        .lock()
        .expect("recording editor lock poisoned")
        .playback();
    let mut audio: Option<WaveAudio> = None;
    let mut audio_cursor = playback.position;
    let mut epoch = Instant::now();
    let mut epoch_position = playback.position;
    let mut render = true;
    let mut exported = false;
    loop {
        if cancel.load(Ordering::Acquire) {
            break;
        }
        let wait = if playback.playing {
            Duration::from_millis(30)
        } else {
            Duration::from_secs(60)
        };
        let command = if render {
            commands.try_recv().ok()
        } else {
            match commands.recv_timeout(wait) {
                Ok(command) => Some(command),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        };
        let mut batch: Vec<Command> = command.into_iter().chain(commands.try_iter()).collect();
        // UI scrubbing can outrun decoding; only the last update before a control command matters.
        let mut latest = None;
        let mut controls = Vec::new();
        for command in batch.drain(..) {
            match command {
                Command::Update(update) => latest = Some(update),
                control => controls.push(control),
            }
        }
        if let Some(update) = latest {
            playback = update;
            playback.position = playback.position.clamp(0, recording.duration_100ns() - 1);
            if let Some(output) = &mut audio
                && let Err(error) = output.reset()
            {
                set_error(&state, &mut playback, error);
            }
            audio_cursor = playback.position;
            epoch_position = playback.position;
            epoch = Instant::now();
            render = true;
        }
        let mut end = false;
        for control in controls {
            match control {
                Command::Cancel => {
                    end = true;
                    break;
                }
                Command::Export {
                    path,
                    format,
                    trim,
                    effects,
                } => {
                    if let Some(output) = &mut audio {
                        let _ = output.reset();
                    }
                    playback.playing = false;
                    let result =
                        recording.export_with_cancel(&path, format, trim, &effects, &cancel);
                    let mut model = state.lock().expect("recording editor lock poisoned");
                    model.exporting = false;
                    model.playing = false;
                    match result {
                        Ok(()) => {
                            model.exported = true;
                            exported = true;
                            let _ = events.send(WorkerEvent::Exported(path));
                            end = true;
                        }
                        Err(error) => {
                            model.error = Some(format!("{error:#}"));
                        }
                    }
                    drop(model);
                    orbment_ui::wake(HWND(wake as *mut _));
                }
                Command::Update(_) => unreachable!(),
            }
        }
        if end {
            break;
        }
        if cancel.load(Ordering::Acquire) {
            break;
        }
        if playback.playing || render {
            let result = (|| -> Result<egui::ColorImage> {
                if playback.playing {
                    if playback.audio {
                        if audio.is_none() {
                            audio = Some(WaveAudio::new()?);
                        }
                        let output = audio.as_mut().expect("audio opened");
                        output.reap()?;
                        playback.position = epoch_position + output.elapsed_100ns()?;
                        while audio_cursor
                            < (playback.position + TICKS_PER_SECOND / 4)
                                .min(playback.trim.end_100ns)
                        {
                            let frames = (((playback.trim.end_100ns - audio_cursor) as i128
                                * 48_000
                                / TICKS_PER_SECOND as i128)
                                as usize)
                                .min(4_800);
                            if frames == 0 {
                                break;
                            }
                            let samples =
                                recording.preview_audio(audio_cursor, frames, &playback.effects)?;
                            output.write(samples)?;
                            audio_cursor += frames as i64 * TICKS_PER_SECOND / 48_000;
                        }
                    } else {
                        playback.position = epoch_position + ticks(epoch.elapsed().as_secs_f64());
                    }
                    if playback.position
                        >= playback.trim.end_100ns - (TICKS_PER_SECOND / 48_000).max(1)
                    {
                        playback.position = playback.trim.end_100ns - 1;
                        playback.playing = false;
                        if let Some(output) = &mut audio {
                            output.reset()?;
                        }
                    }
                }
                preview_image(recording.preview_frame(
                    playback.position.min(recording.duration_100ns() - 1),
                    &playback.effects,
                )?)
            })();
            match result {
                Ok(frame) => {
                    let mut model = state.lock().expect("recording editor lock poisoned");
                    model.position = playback.position;
                    model.playing = playback.playing;
                    model.preview = Some(frame);
                }
                Err(error) => {
                    if let Some(output) = &mut audio {
                        let _ = output.reset();
                    }
                    set_error(&state, &mut playback, error);
                }
            }
            render = false;
            orbment_ui::wake(HWND(wake as *mut _));
        }
    }
    drop(audio);
    if !exported {
        if let Err(error) = recording.cancel() {
            tracing::warn!(%error, "Cancel recording editor session");
        }
        let _ = events.send(WorkerEvent::Closed);
    }
    orbment_ui::wake(HWND(wake as *mut _));
}
fn set_error(state: &Mutex<EditorState>, playback: &mut Playback, error: anyhow::Error) {
    playback.playing = false;
    let mut model = state.lock().expect("recording editor lock poisoned");
    model.playing = false;
    model.error = Some(format!("{error:#}"));
}

struct AudioBuffer {
    _samples: Box<[i16]>,
    header: Box<WAVEHDR>,
}
struct WaveAudio {
    device: HWAVEOUT,
    buffers: VecDeque<AudioBuffer>,
}
impl WaveAudio {
    fn new() -> Result<Self> {
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: 2,
            nSamplesPerSec: 48_000,
            nAvgBytesPerSec: 192_000,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        let mut device = HWAVEOUT::default();
        mm(
            unsafe {
                waveOutOpen(
                    Some(&mut device),
                    WAVE_MAPPER,
                    &format,
                    None,
                    None,
                    CALLBACK_NULL,
                )
            },
            "waveOutOpen",
        )?;
        Ok(Self {
            device,
            buffers: VecDeque::new(),
        })
    }
    fn write(&mut self, samples: Vec<f32>) -> Result<()> {
        ensure!(
            samples.len().is_multiple_of(2) && !samples.is_empty(),
            "Invalid stereo preview audio"
        );
        let mut samples: Box<[i16]> = samples
            .into_iter()
            .map(|sample| (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
            .collect();
        let mut header = Box::new(WAVEHDR {
            lpData: PSTR(samples.as_mut_ptr().cast()),
            dwBufferLength: u32::try_from(samples.len() * size_of::<i16>())?,
            ..Default::default()
        });
        mm(
            unsafe { waveOutPrepareHeader(self.device, &mut *header, size_of::<WAVEHDR>() as u32) },
            "waveOutPrepareHeader",
        )?;
        let result = mm(
            unsafe { waveOutWrite(self.device, &mut *header, size_of::<WAVEHDR>() as u32) },
            "waveOutWrite",
        );
        if let Err(error) = result {
            let status = unsafe {
                waveOutUnprepareHeader(self.device, &mut *header, size_of::<WAVEHDR>() as u32)
            };
            if status != 0 {
                std::mem::forget(AudioBuffer {
                    _samples: samples,
                    header,
                });
            }
            return Err(error);
        }
        self.buffers.push_back(AudioBuffer {
            _samples: samples,
            header,
        });
        Ok(())
    }
    fn reap(&mut self) -> Result<()> {
        while self.buffers.front().is_some_and(|buffer| unsafe { std::ptr::addr_of!(buffer.header.dwFlags).read_volatile() } & WHDR_DONE != 0) {
            let buffer = self.buffers.front_mut().expect("finished audio buffer");
            mm(unsafe { waveOutUnprepareHeader(self.device, &mut *buffer.header, size_of::<WAVEHDR>() as u32) }, "waveOutUnprepareHeader")?;
            self.buffers.pop_front();
        }
        Ok(())
    }
    fn reset(&mut self) -> Result<()> {
        mm(unsafe { waveOutReset(self.device) }, "waveOutReset")?;
        self.reap()
    }
    fn elapsed_100ns(&self) -> Result<i64> {
        let mut time = MMTIME {
            wType: TIME_SAMPLES,
            ..Default::default()
        };
        mm(
            unsafe { waveOutGetPosition(self.device, &mut time, size_of::<MMTIME>() as u32) },
            "waveOutGetPosition",
        )?;
        let value = unsafe {
            match time.wType {
                TIME_SAMPLES => i64::from(time.u.sample) * TICKS_PER_SECOND / 48_000,
                TIME_BYTES => i64::from(time.u.cb) * TICKS_PER_SECOND / 192_000,
                TIME_MS => i64::from(time.u.ms) * 10_000,
                _ => bail!("Audio device returned an unsupported playback position format"),
            }
        };
        Ok(value)
    }
}
impl Drop for WaveAudio {
    fn drop(&mut self) {
        let _ = unsafe { waveOutReset(self.device) };
        for mut buffer in self.buffers.drain(..) {
            let result = unsafe {
                waveOutUnprepareHeader(
                    self.device,
                    &mut *buffer.header,
                    size_of::<WAVEHDR>() as u32,
                )
            };
            // A failed driver release must not leave it referencing freed sample/header memory.
            if result != 0 {
                std::mem::forget(buffer);
            }
        }
        let _ = unsafe { waveOutClose(self.device) };
    }
}
fn mm(status: u32, operation: &str) -> Result<()> {
    ensure!(
        status == 0,
        "{operation} failed with multimedia error {status}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_is_bounded_before_upload_and_preserves_aspect_ratio() {
        for (width, height, expected) in [
            (3840, 2160, [960, 540]),
            (1080, 1920, [304, 540]),
            (2000, 1, [960, 1]),
            (2, 1, [2, 1]),
        ] {
            let preview = preview_image(CapturedImage {
                width,
                height,
                pixels: [10, 20, 30, 128].repeat(width as usize * height as usize),
            })
            .unwrap();
            assert_eq!(preview.size, expected);
            assert!(preview.pixels.len() <= 960 * 540);
            assert!(preview.pixels.iter().all(|pixel| {
                *pixel == egui::Color32::from_rgba_unmultiplied(30, 20, 10, 128)
            }));
        }
    }

    #[test]
    fn preview_samples_rows_without_swapping_axes_or_color_channels() {
        let frame = CapturedImage {
            width: 2,
            height: 2,
            pixels: vec![
                0, 0, 255, 255, 0, 255, 0, 255, 255, 0, 0, 255, 255, 255, 255, 255,
            ],
        };
        let preview = preview_image(frame).unwrap();
        assert_eq!(
            preview.pixels,
            [
                egui::Color32::RED,
                egui::Color32::GREEN,
                egui::Color32::BLUE,
                egui::Color32::WHITE,
            ]
        );
    }
}
