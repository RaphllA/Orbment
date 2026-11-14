use super::Clock;
use anyhow::{Result, ensure};
use std::{
    collections::VecDeque,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};
use windows::Win32::{
    Media::Audio::*,
    System::Com::{CLSCTX_ALL, CoCreateInstance},
};

pub const SAMPLE_RATE: i64 = 48_000;
const TICKS: i64 = 10_000_000;

pub struct AudioCapture {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    clock: Clock,
}
impl AudioCapture {
    pub fn start(system: bool, clock: Clock) -> Result<Self> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let device = enumerator
                .GetDefaultAudioEndpoint(if system { eRender } else { eCapture }, eConsole)?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            // WASAPI's shared-mode converter supplies a common stereo float format;
            // both tracks retain the device's QPC timestamps, never a wall clock.
            let format = WAVEFORMATEX {
                wFormatTag: 3,
                nChannels: 2,
                nSamplesPerSec: 48_000,
                nAvgBytesPerSec: 384_000,
                nBlockAlign: 8,
                wBitsPerSample: 32,
                cbSize: 0,
            };
            let flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY
                | if system {
                    AUDCLNT_STREAMFLAGS_LOOPBACK
                } else {
                    0
                };
            client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 2_000_000, 0, &format, None)?;
            let capture: IAudioCaptureClient = client.GetService()?;
            client.Start()?;
            Ok(Self {
                client,
                capture,
                clock,
            })
        }
    }
    pub fn drain(&self, mut packet: impl FnMut(i64, &[f32]) -> Result<()>) -> Result<()> {
        unsafe {
            while self.capture.GetNextPacketSize()? != 0 {
                let mut data = std::ptr::null_mut();
                let mut frames = 0;
                let mut flags = 0;
                let mut qpc = 0;
                self.capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))?;
                let time = if flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 != 0 {
                    self.clock.now()? - i64::from(frames) * TICKS / SAMPLE_RATE
                } else {
                    qpc as i64 - self.clock.epoch_100ns()
                };
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                let values = if silent {
                    vec![0.0; frames as usize * 2]
                } else {
                    std::slice::from_raw_parts(data.cast::<f32>(), frames as usize * 2).to_vec()
                };
                // Always release the engine buffer before touching disk or the encoder.
                self.capture.ReleaseBuffer(frames)?;
                packet(time, &values)?;
            }
        }
        Ok(())
    }
}
impl Drop for AudioCapture {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

/// PCM tracks are seekable stereo f32/48k, with file offset zero at the session QPC epoch.
/// Gaps from endpoint silence remain zero; audio never drifts with video frame cadence.
pub struct AudioTrack {
    file: File,
}
impl AudioTrack {
    pub fn create(path: &Path) -> Result<Self> {
        Ok(Self {
            file: OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(path)?,
        })
    }
    pub fn packet(&mut self, time: i64, samples: &[f32]) -> Result<()> {
        let frame = (i128::from(time) * i128::from(SAMPLE_RATE) / i128::from(TICKS)) as i64;
        let skip = (-frame).max(0) as usize * 2;
        if skip >= samples.len() {
            return Ok(());
        }
        self.file.seek(SeekFrom::Start(frame.max(0) as u64 * 8))?;
        let bytes: Vec<u8> = samples[skip..]
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        self.file.write_all(&bytes)?;
        Ok(())
    }
    pub fn finish(mut self, duration: i64) -> Result<()> {
        self.file
            .set_len((duration as u64 * SAMPLE_RATE as u64 / TICKS as u64) * 8)?;
        self.file.flush()?;
        Ok(())
    }
}

pub struct TrackMixer {
    tracks: Vec<(File, f32)>,
}
impl TrackMixer {
    pub fn open(directory: &Path, system_gain: f32, microphone_gain: f32) -> Result<Self> {
        ensure!(
            system_gain.is_finite() && microphone_gain.is_finite(),
            "invalid audio gain"
        );
        let mut tracks = Vec::new();
        for (name, gain) in [
            ("system.f32", system_gain),
            ("microphone.f32", microphone_gain),
        ] {
            let path = directory.join(name);
            if path.exists() {
                tracks.push((File::open(path)?, gain));
            }
        }
        Ok(Self { tracks })
    }
    pub fn has_audio(&self) -> bool {
        !self.tracks.is_empty()
    }
    pub fn read(&mut self, first_frame: u64, frames: usize) -> Result<Vec<f32>> {
        let mut mixed = vec![0.0; frames * 2];
        let mut bytes = vec![0; frames * 8];
        for (file, gain) in &mut self.tracks {
            bytes.fill(0);
            file.seek(SeekFrom::Start(first_frame * 8))?;
            let mut read = 0;
            while read < bytes.len() {
                let count = file.read(&mut bytes[read..])?;
                if count == 0 {
                    break;
                }
                read += count;
            }
            for (output, value) in mixed.iter_mut().zip(bytes.as_chunks::<4>().0) {
                *output += f32::from_le_bytes(*value) * *gain;
            }
        }
        for sample in &mut mixed {
            *sample = sample.clamp(-1.0, 1.0);
        }
        Ok(mixed)
    }
}

pub struct LiveMixer {
    first_frame: i64,
    values: VecDeque<f32>,
}
impl LiveMixer {
    pub fn new() -> Self {
        Self {
            first_frame: 0,
            values: VecDeque::new(),
        }
    }
    pub fn packet(&mut self, time: i64, values: &[f32]) -> Result<()> {
        let frame = (i128::from(time) * i128::from(SAMPLE_RATE) / i128::from(TICKS)) as i64;
        let offset = frame - self.first_frame;
        let skip = (-offset).max(0) as usize * 2;
        if skip >= values.len() {
            return Ok(());
        }
        let offset = offset.max(0) as usize * 2;
        let length = offset + values.len() - skip;
        ensure!(
            length <= SAMPLE_RATE as usize * 2 * 10,
            "audio encoder stalled for more than ten seconds"
        );
        self.values.resize(self.values.len().max(length), 0.0);
        for (i, value) in values[skip..].iter().enumerate() {
            self.values[offset + i] += *value;
        }
        Ok(())
    }
    pub fn take_until(&mut self, time: i64) -> Option<(i64, Vec<f32>)> {
        let end = time.max(0) * SAMPLE_RATE / TICKS;
        let frames = (end - self.first_frame).max(0) as usize;
        if frames == 0 {
            return None;
        }
        let time = self.first_frame * TICKS / SAMPLE_RATE;
        self.first_frame = end;
        let samples = (0..frames * 2)
            .map(|_| self.values.pop_front().unwrap_or(0.0).clamp(-1.0, 1.0))
            .collect();
        Some((time, samples))
    }
}
