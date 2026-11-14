use crate::{CapturedImage, Rect};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(windows)]
#[path = "recorder_audio.rs"]
mod audio;
#[path = "recorder_effects.rs"]
mod effects;
#[path = "recorder_export.rs"]
mod export;
#[cfg(windows)]
#[path = "recorder_media.rs"]
mod media;
#[cfg(windows)]
#[path = "recorder_video.rs"]
mod video;
pub use effects::{EffectOptions, KeyBubble, KeyCategory, PointerVisual, Timeline, VisualFrame};

pub const TICKS_PER_SECOND: i64 = 10_000_000;

#[derive(Clone, Debug)]
pub struct RecordingOptions {
    pub region: Rect,
    pub fps: u32,
    pub bitrate: u32,
    pub system_audio: bool,
    pub microphone: bool,
    /// Bake the native cursor into frames; leave disabled for editable pointer tracks.
    pub show_cursor: bool,
    pub record_mouse: bool,
    pub record_keyboard: bool,
    pub quick: bool,
}
impl Default for RecordingOptions {
    fn default() -> Self {
        Self {
            region: Rect {
                x: 0.0,
                y: 0.0,
                width: 1280.0,
                height: 720.0,
            },
            fps: 24,
            bitrate: 8_000_000,
            system_audio: true,
            microphone: false,
            show_cursor: false,
            record_mouse: true,
            record_keyboard: true,
            quick: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum PointerButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub enum PointerAction {
    Move,
    Down(PointerButton),
    Up(PointerButton),
    Wheel { delta: i16, horizontal: bool },
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum InputEvent {
    Pointer {
        x: f64,
        y: f64,
        action: PointerAction,
    },
    /// Modifier bits: Ctrl=1, Shift=2, Alt=4, Win=8. Labels are resolved off the hook thread.
    Key {
        virtual_key: u32,
        scan_code: u32,
        down: bool,
        modifiers: u8,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimedEvent {
    pub time_100ns: i64,
    pub event: InputEvent,
}

#[derive(Clone, Copy, Debug)]
pub struct Clock {
    epoch: i64,
    frequency: i64,
}
impl Clock {
    #[cfg(windows)]
    pub fn start() -> Result<Self> {
        use windows::Win32::System::Performance::{
            QueryPerformanceCounter, QueryPerformanceFrequency,
        };
        let mut epoch = 0;
        let mut frequency = 0;
        unsafe {
            QueryPerformanceFrequency(&mut frequency)?;
            QueryPerformanceCounter(&mut epoch)?;
        }
        ensure!(frequency > 0, "invalid QPC frequency");
        Ok(Self { epoch, frequency })
    }
    #[cfg(windows)]
    pub fn now(self) -> Result<i64> {
        let mut tick = 0;
        unsafe {
            windows::Win32::System::Performance::QueryPerformanceCounter(&mut tick)?;
        }
        Ok(
            ((i128::from(tick - self.epoch) * i128::from(TICKS_PER_SECOND))
                / i128::from(self.frequency)) as i64,
        )
    }
    pub fn epoch_100ns(self) -> i64 {
        (i128::from(self.epoch) * i128::from(TICKS_PER_SECOND) / i128::from(self.frequency)) as i64
    }
}

#[derive(Clone)]
pub struct EventSink {
    sender: SyncSender<TimedEvent>,
    clock: Clock,
    dropped: Arc<AtomicU64>,
    paused: Arc<AtomicBool>,
    mouse: bool,
    keyboard: bool,
}
impl EventSink {
    /// Safe in the shared low-level hook: QPC plus bounded nonblocking enqueue only.
    #[cfg(windows)]
    pub fn record(&self, event: InputEvent) -> bool {
        if self.paused.load(Ordering::Acquire) {
            return true;
        }
        if !match event {
            InputEvent::Pointer { .. } => self.mouse,
            InputEvent::Key { .. } => self.keyboard,
        } {
            return true;
        }
        let Ok(time_100ns) = self.clock.now() else {
            return false;
        };
        if self
            .sender
            .try_send(TimedEvent { time_100ns, event })
            .is_err()
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            false
        } else {
            true
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ExportFormat {
    Mp4,
    Gif,
    WebP,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct TrimRange {
    pub start_100ns: i64,
    pub end_100ns: i64,
}
impl TrimRange {
    fn validate(self, duration: i64) -> Result<()> {
        ensure!(
            self.start_100ns >= 0
                && self.start_100ns < self.end_100ns
                && self.end_100ns <= duration,
            "trim must be a nonempty range within the recording"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Segment {
    file: String,
    start_100ns: i64,
    end_100ns: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct StillFrame {
    file: String,
    time_100ns: i64,
    duration_100ns: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct SessionManifest {
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    origin: [f64; 2],
    duration_100ns: i64,
    quick: bool,
    qpc_epoch: i64,
    qpc_frequency: i64,
    audio_sample_rate: u32,
    audio_channels: u16,
    segments: Vec<Segment>,
    still_frames: Vec<StillFrame>,
    media_unavailable: Option<String>,
}

struct SessionDirectory {
    path: PathBuf,
    lock: Option<File>,
}
impl SessionDirectory {
    #[cfg(windows)]
    fn create(root: &Path) -> Result<Self> {
        use std::os::windows::fs::OpenOptionsExt;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        fs::create_dir_all(root)?;
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = root.join(format!(
            "recording-{}-{timestamp}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path)?;
        let lock = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .share_mode(0)
            .open(path.join("active.lock"))
        {
            Ok(lock) => lock,
            Err(error) => {
                let _ = fs::remove_dir(&path);
                return Err(error.into());
            }
        };
        Ok(Self {
            path,
            lock: Some(lock),
        })
    }
    fn cleanup(&mut self) -> Result<()> {
        self.lock.take();
        if self.path.exists() {
            fs::remove_dir_all(&self.path)?;
        }
        Ok(())
    }
}
impl Drop for SessionDirectory {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

/// Call once at startup. An exclusive lease protects recordings in another live instance.
#[cfg(windows)]
pub fn cleanup_stale_sessions(root: &Path) -> Result<usize> {
    use std::os::windows::fs::OpenOptionsExt;
    if !root.exists() {
        return Ok(0);
    }
    let mut removed = 0;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir()
            || !entry
                .file_name()
                .to_string_lossy()
                .starts_with("recording-")
        {
            continue;
        }
        match OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(0)
            .open(entry.path().join("active.lock"))
        {
            Ok(lock) => {
                drop(lock);
                fs::remove_dir_all(entry.path())?;
                removed += 1;
            }
            Err(error) if error.raw_os_error() == Some(32) || error.raw_os_error() == Some(33) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(removed)
}

pub struct RecordingSession {
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    directory: PathBuf,
    worker: Option<JoinHandle<Result<RecordedSession>>>,
    events: EventSink,
}
impl RecordingSession {
    #[cfg(windows)]
    pub fn start(root: &Path, options: RecordingOptions) -> Result<Self> {
        Self::start_with_wake(root, options, || {})
    }
    #[cfg(windows)]
    pub fn start_with_wake(
        root: &Path,
        options: RecordingOptions,
        wake: impl FnOnce() + Send + 'static,
    ) -> Result<Self> {
        ensure!(
            options.fps >= 1 && options.fps <= 240 && options.bitrate >= 100_000,
            "invalid recording frame rate or bitrate"
        );
        ensure!(
            [
                options.region.x,
                options.region.y,
                options.region.width,
                options.region.height
            ]
            .iter()
            .all(|v| v.is_finite()),
            "nonfinite recording region"
        );
        ensure!(
            options.region.width >= 2.0
                && options.region.height >= 2.0
                && options.region.width <= 16384.0
                && options.region.height <= 16384.0,
            "recording region dimensions out of range"
        );
        let directory = SessionDirectory::create(root)?;
        let directory_path = directory.path.clone();
        let (sender, receiver) = mpsc::sync_channel(8192);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let paused = Arc::new(AtomicBool::new(false));
        let worker_paused = paused.clone();
        let finished = Arc::new(AtomicBool::new(false));
        let worker_finished = finished.clone();
        let dropped = Arc::new(AtomicU64::new(0));
        let worker_dropped = dropped.clone();
        let mouse = options.record_mouse && !options.quick;
        let keyboard = options.record_keyboard && !options.quick;
        let worker = thread::Builder::new()
            .name("orbment-recorder".into())
            .spawn(move || {
                let result = record_worker(
                    directory,
                    options,
                    receiver,
                    worker_stop,
                    worker_paused,
                    worker_dropped,
                    &ready_sender,
                );
                if let Err(error) = &result {
                    let _ = ready_sender.try_send(Err(format!("{error:#}")));
                }
                worker_finished.store(true, Ordering::Release);
                wake();
                result
            })?;
        match ready_receiver.recv() {
            Ok(Ok(clock)) => Ok(Self {
                stop,
                paused: paused.clone(),
                finished,
                directory: directory_path,
                worker: Some(worker),
                events: EventSink {
                    sender,
                    clock,
                    dropped,
                    paused,
                    mouse,
                    keyboard,
                },
            }),
            error => {
                stop.store(true, Ordering::Release);
                let result = worker
                    .join()
                    .map_err(|_| anyhow::anyhow!("recording initialization panicked"))?;
                result?;
                bail!("recording initialization failed: {error:?}")
            }
        }
    }
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Release);
    }
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }
    pub fn event_sink(&self) -> EventSink {
        self.events.clone()
    }
    #[cfg(windows)]
    pub fn record_event(&self, event: InputEvent) -> bool {
        self.events.record(event)
    }
    pub fn stop(mut self) -> Result<RecordedSession> {
        self.stop.store(true, Ordering::Release);
        let result = self
            .worker
            .take()
            .context("recording already stopped")
            .and_then(|worker| {
                worker
                    .join()
                    .map_err(|_| anyhow::anyhow!("recording worker panicked"))?
            });
        if let Err(error) = result {
            // Retry after joining, when all worker-owned file handles have been released.
            if let Err(cleanup) = fs::remove_dir_all(&self.directory)
                && cleanup.kind() != std::io::ErrorKind::NotFound
            {
                return Err(error.context(format!(
                    "Remove failed recording session {}: {cleanup}",
                    self.directory.display()
                )));
            }
            return Err(error);
        }
        result
    }
    pub fn cancel(self) -> Result<()> {
        let mut recorded = self.stop()?;
        recorded.cancel()
    }
}
impl Drop for RecordingSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub struct RecordedSession {
    directory: SessionDirectory,
    manifest: SessionManifest,
    timeline: Timeline,
    exported: bool,
}
impl RecordedSession {
    pub fn duration_100ns(&self) -> i64 {
        self.manifest.duration_100ns
    }
    pub fn full_range(&self) -> TrimRange {
        TrimRange {
            start_100ns: 0,
            end_100ns: self.duration_100ns(),
        }
    }
    pub fn timeline(&self) -> &Timeline {
        &self.timeline
    }
    pub fn media_unavailable(&self) -> Option<&str> {
        self.manifest.media_unavailable.as_deref()
    }
    pub fn dimensions(&self) -> (u32, u32) {
        (self.manifest.width, self.manifest.height)
    }
    pub fn session_directory(&self) -> &Path {
        &self.directory.path
    }
    pub fn cancel(&mut self) -> Result<()> {
        self.directory.cleanup()?;
        self.exported = true;
        Ok(())
    }

    /// Runs on an MTA worker. Preview and export use the identical effect compositor.
    #[cfg(windows)]
    pub fn preview_frame(&self, time_100ns: i64, options: &EffectOptions) -> Result<CapturedImage> {
        ensure!(
            !self.exported,
            "recording session already exported or cancelled"
        );
        ensure!(
            time_100ns >= 0 && time_100ns < self.duration_100ns(),
            "preview timestamp outside recording"
        );
        options.validate()?;
        let _com = media::ComApartment::new()?;
        let mut image = export::read_frame(self, time_100ns)?;
        effects::compose(&mut image, &self.timeline.at(time_100ns, options), options)?;
        Ok(image)
    }
    /// Stereo f32/48k blocks for the playback surface's audio renderer.
    #[cfg(windows)]
    pub fn preview_audio(
        &self,
        time_100ns: i64,
        frames: usize,
        options: &EffectOptions,
    ) -> Result<Vec<f32>> {
        ensure!(
            !self.exported && !self.manifest.quick,
            "no editable audio tracks in this session"
        );
        ensure!(
            time_100ns >= 0 && time_100ns < self.duration_100ns() && frames <= 480_000,
            "preview audio request exceeds recording or ten-second buffer limit"
        );
        options.validate()?;
        let first = (i128::from(time_100ns) * 48_000 / i128::from(TICKS_PER_SECOND)) as u64;
        audio::TrackMixer::open(
            &self.directory.path,
            options.system_audio_gain,
            options.microphone_gain,
        )?
        .read(first, frames)
    }
    #[cfg(windows)]
    pub fn export(
        &mut self,
        path: &Path,
        format: ExportFormat,
        trim: TrimRange,
        options: &EffectOptions,
    ) -> Result<()> {
        self.export_with_cancel(path, format, trim, options, &AtomicBool::new(false))
    }
    #[cfg(windows)]
    pub fn export_with_cancel(
        &mut self,
        path: &Path,
        format: ExportFormat,
        trim: TrimRange,
        options: &EffectOptions,
        cancel: &AtomicBool,
    ) -> Result<()> {
        ensure!(
            !self.exported && !self.manifest.quick,
            "recording is unavailable for recomposition; quick recordings use direct export"
        );
        trim.validate(self.duration_100ns())?;
        options.validate()?;
        let result = export::export(self, path, format, trim, options, cancel);
        if cancel.load(Ordering::Acquire) {
            self.cancel()?;
        }
        result?;
        self.directory.cleanup()?;
        self.exported = true;
        Ok(())
    }
    pub fn export_quick(&mut self, path: &Path) -> Result<()> {
        ensure!(
            !self.exported && self.manifest.quick,
            "not an active quick recording"
        );
        ensure!(!path.exists(), "export destination already exists");
        export::validate_destination(path, &self.directory.path)?;
        let source = self.directory.path.join("quick.mp4");
        let mut output = OpenOptions::new().write(true).create_new(true).open(path)?;
        let result = (|| -> Result<()> {
            std::io::copy(&mut File::open(source)?, &mut output)?;
            output.flush()?;
            output.sync_all()?;
            Ok(())
        })();
        drop(output);
        if result.is_err() {
            let _ = fs::remove_file(path);
        }
        result?;
        self.directory.cleanup()?;
        self.exported = true;
        Ok(())
    }
}

#[cfg(windows)]
pub fn hide_from_capture(hwnd: isize) -> Result<()> {
    use windows::Win32::{
        Foundation::HWND,
        UI::WindowsAndMessaging::{SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE},
    };
    unsafe {
        SetWindowDisplayAffinity(HWND(hwnd as *mut _), WDA_EXCLUDEFROMCAPTURE)?;
    }
    Ok(())
}

#[cfg(windows)]
fn record_worker(
    directory: SessionDirectory,
    options: RecordingOptions,
    events: Receiver<TimedEvent>,
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
    ready: &SyncSender<std::result::Result<Clock, String>>,
) -> Result<RecordedSession> {
    use crate::capture_overlay::{CaptureSource, CaptureStream};
    let _com = media::ComApartment::new()?;
    let (runtime, media_unavailable) = match media::MediaRuntime::load() {
        Ok(runtime) => (Some(runtime), None),
        Err(error) if !options.quick => (None, Some(format!("{error:#}"))),
        Err(error) => {
            return Err(
                error.context("quick MP4 recording requires the Windows Media Feature Pack")
            );
        }
    };
    // H.264 requires even sizes. Crop only the final right/bottom pixel, never rescale.
    let width = options.region.width.floor() as u32 & !1;
    let height = options.region.height.floor() as u32 & !1;
    let region = Rect {
        x: options.region.x.floor(),
        y: options.region.y.floor(),
        width: f64::from(width),
        height: f64::from(height),
    };
    let writer = if let Some(runtime) = &runtime {
        Some(media::VideoWriter::new(
            runtime.clone(),
            &directory.path.join(if options.quick {
                "quick.mp4"
            } else {
                "video-000000.mp4"
            }),
            width,
            height,
            options.fps,
            options.bitrate,
            options.quick && (options.system_audio || options.microphone),
        )?)
    } else {
        None
    };
    let clock = Clock::start()?;
    let capture = CaptureStream::start_with_options(
        CaptureSource::Desktop(region),
        options.fps,
        options.show_cursor,
    )?;
    let system = if options.system_audio {
        Some(audio::AudioCapture::start(true, clock).context("start system loopback audio")?)
    } else {
        None
    };
    let microphone = if options.microphone {
        Some(audio::AudioCapture::start(false, clock).context("start microphone audio")?)
    } else {
        None
    };
    let mut system_track = if options.system_audio && !options.quick {
        Some(audio::AudioTrack::create(
            &directory.path.join("system.f32"),
        )?)
    } else {
        None
    };
    let mut microphone_track = if options.microphone && !options.quick {
        Some(audio::AudioTrack::create(
            &directory.path.join("microphone.f32"),
        )?)
    } else {
        None
    };
    let mut live_mix = audio::LiveMixer::new();
    let mut pointer_events = BufWriter::new(File::create(directory.path.join("pointer.jsonl"))?);
    let mut key_events = BufWriter::new(File::create(directory.path.join("keyboard.jsonl"))?);
    let manifest = SessionManifest {
        width,
        height,
        fps: options.fps,
        bitrate: options.bitrate,
        origin: [region.x, region.y],
        duration_100ns: 0,
        quick: options.quick,
        qpc_epoch: clock.epoch,
        qpc_frequency: clock.frequency,
        audio_sample_rate: 48_000,
        audio_channels: 2,
        segments: Vec::new(),
        still_frames: Vec::new(),
        media_unavailable,
    };
    let mut video = video::VideoTrack::new(&directory.path, manifest, runtime, writer);
    let mut recorded_events = Vec::new();
    if options.record_mouse && !options.quick {
        let mut point = windows::Win32::Foundation::POINT::default();
        unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut point)?;
        }
        let event = TimedEvent {
            time_100ns: 0,
            event: InputEvent::Pointer {
                x: f64::from(point.x) - region.x,
                y: f64::from(point.y) - region.y,
                action: PointerAction::Move,
            },
        };
        serde_json::to_writer(&mut pointer_events, &event)?;
        pointer_events.write_all(b"\n")?;
        recorded_events.push(event);
    }
    let mut announced = false;
    let mut pause_started = None;
    let mut paused_ticks = 0;
    let mut resumed_at = 0;
    loop {
        let raw_now = clock.now()?;
        if let Some(error) = capture.take_error() {
            bail!("screen capture stopped: {error}");
        }
        if paused.load(Ordering::Acquire) {
            pause_started.get_or_insert(raw_now);
            let _ = capture.latest_timed();
            for source in [&system, &microphone].into_iter().flatten() {
                source.drain(|_, _| Ok(()))?;
            }
            events.try_iter().for_each(drop);
            if stop.load(Ordering::Acquire) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
            continue;
        }
        if let Some(started) = pause_started.take() {
            paused_ticks += raw_now - started;
            resumed_at = raw_now;
        }
        let now = raw_now - paused_ticks;
        if let Some((captured_at, image)) = capture.latest_timed() {
            let time = captured_at - clock.epoch_100ns();
            if time >= resumed_at {
                video.push(time - paused_ticks, image)?;
            }
        }
        if !announced && video.has_frame() {
            ready
                .send(Ok(clock))
                .map_err(|_| anyhow::anyhow!("recording startup receiver disappeared"))?;
            announced = true;
        }
        let initial_deadline = 20_000_000 + 4 * TICKS_PER_SECOND / i64::from(options.fps);
        ensure!(
            video.has_frame() || now < initial_deadline,
            "screen capture produced no initial frame before its staging deadline"
        );
        video.advance_static(now)?;
        for (source, track) in [
            (&system, &mut system_track),
            (&microphone, &mut microphone_track),
        ] {
            if let Some(source) = source {
                source.drain(|time, samples| {
                    if time < resumed_at {
                        return Ok(());
                    }
                    let time = time - paused_ticks;
                    if options.quick {
                        live_mix.packet(time, samples)
                    } else if let Some(track) = track {
                        track.packet(time, samples)
                    } else {
                        Ok(())
                    }
                })?;
            }
        }
        if options.quick
            && video.has_frame()
            && (options.system_audio || options.microphone)
            && let Some((time, samples)) = live_mix.take_until(now - 1_000_000)
        {
            video.audio(&samples, time)?;
        }
        for mut event in events.try_iter() {
            if event.time_100ns < resumed_at {
                continue;
            }
            event.time_100ns -= paused_ticks;
            if let InputEvent::Pointer { x, y, .. } = &mut event.event {
                *x -= region.x;
                *y -= region.y;
                if !x.is_finite() || !y.is_finite() {
                    continue;
                }
            }
            let output = match event.event {
                InputEvent::Pointer { .. } => &mut pointer_events,
                InputEvent::Key { .. } => &mut key_events,
            };
            serde_json::to_writer(&mut *output, &event)?;
            output.write_all(b"\n")?;
            recorded_events.push(event);
        }
        ensure!(
            dropped.load(Ordering::Relaxed) == 0,
            "recording input event queue overflowed; no incomplete event track will be exported"
        );
        if stop.load(Ordering::Acquire) && video.has_frame() {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    let raw_end = clock.now()?;
    let duration = video.end_time(pause_started.unwrap_or(raw_end) - paused_ticks);
    if options.quick
        && (options.system_audio || options.microphone)
        && let Some((time, samples)) = live_mix.take_until(duration)
    {
        video.audio(&samples, time)?;
    }
    let manifest = video.finish(duration)?;
    if let Some(track) = system_track {
        track.finish(duration)?;
    }
    if let Some(track) = microphone_track {
        track.finish(duration)?;
    }
    pointer_events.flush()?;
    key_events.flush()?;
    serde_json::to_writer_pretty(
        File::create(directory.path.join("session.json"))?,
        &manifest,
    )?;
    Ok(RecordedSession {
        directory,
        manifest,
        timeline: Timeline::new(recorded_events),
        exported: false,
    })
}

/// Read persisted event tracks for an external playback surface; no hooks are needed during replay.
pub fn read_event_tracks(directory: &Path) -> Result<Timeline> {
    let mut events = Vec::new();
    for name in ["pointer.jsonl", "keyboard.jsonl"] {
        for line in BufReader::new(File::open(directory.join(name))?).lines() {
            events.push(serde_json::from_str::<TimedEvent>(&line?)?);
        }
    }
    Ok(Timeline::new(events))
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::os::windows::fs::OpenOptionsExt;

    #[test]
    fn cancelling_failed_recording_retries_cleanup_after_worker_handles_close() {
        let root = std::env::temp_dir().join(format!(
            "orbment-recording-cancel-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let directory = SessionDirectory::create(&root).unwrap();
        let path = directory.path.clone();
        let worker_path = path.clone();
        let worker = thread::spawn(move || -> Result<RecordedSession> {
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .share_mode(0)
                .open(worker_path.join("video.mp4"))
                .unwrap();
            drop(directory);
            assert!(worker_path.exists());
            drop(file);
            bail!("capture failed");
        });
        let (sender, _receiver) = mpsc::sync_channel(1);
        let session = RecordingSession {
            stop: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            finished: Arc::new(AtomicBool::new(false)),
            directory: path.clone(),
            worker: Some(worker),
            events: EventSink {
                sender,
                clock: Clock {
                    epoch: 0,
                    frequency: 1,
                },
                dropped: Arc::new(AtomicU64::new(0)),
                paused: Arc::new(AtomicBool::new(false)),
                mouse: false,
                keyboard: false,
            },
        };
        let error = session.cancel().unwrap_err();
        assert_eq!(error.to_string(), "capture failed");
        assert!(!path.exists());
        fs::remove_dir(root).unwrap();
    }
    #[test]
    fn paused_recording_discards_hook_events_and_resumes_enqueueing() -> Result<()> {
        let (sender, receiver) = mpsc::sync_channel(1);
        let paused = Arc::new(AtomicBool::new(false));
        let session = RecordingSession {
            stop: Arc::new(AtomicBool::new(false)),
            paused: paused.clone(),
            finished: Arc::new(AtomicBool::new(false)),
            directory: PathBuf::new(),
            worker: None,
            events: EventSink {
                sender,
                clock: Clock::start()?,
                dropped: Arc::new(AtomicU64::new(0)),
                paused,
                mouse: true,
                keyboard: true,
            },
        };
        let event = InputEvent::Pointer {
            x: 20.,
            y: 30.,
            action: PointerAction::Move,
        };
        session.set_paused(true);
        for _ in 0..10 {
            assert!(session.record_event(event.clone()));
        }
        assert!(session.is_paused());
        assert!(receiver.try_recv().is_err());
        session.set_paused(false);
        assert!(session.record_event(event.clone()));
        assert_eq!(receiver.try_recv()?.event, event);
        assert_eq!(session.events.dropped.load(Ordering::Relaxed), 0);
        Ok(())
    }
}
