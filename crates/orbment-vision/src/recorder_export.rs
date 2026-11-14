use super::{EffectOptions, ExportFormat, RecordedSession, TICKS_PER_SECOND, TrimRange, effects};
#[cfg(windows)]
use super::{audio, media};
use crate::CapturedImage;
use anyhow::{Context, Result, ensure};
use image::{ExtendedColorType, codecs::webp::WebPEncoder};
use std::{
    fs::{self, File, OpenOptions},
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

fn rgba(image: &CapturedImage) -> Result<Vec<u8>> {
    ensure!(
        image.pixels.len() as u64 == u64::from(image.width) * u64::from(image.height) * 4,
        "invalid export BGRA image"
    );
    let mut output = image.pixels.clone();
    for pixel in output.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
        pixel[3] = 255;
    }
    Ok(output)
}

pub fn write_still_webp(path: &Path, image: &CapturedImage) -> Result<()> {
    let file = File::create(path)?;
    WebPEncoder::new_lossless(file).encode(
        &rgba(image)?,
        image.width,
        image.height,
        ExtendedColorType::Rgba8,
    )?;
    Ok(())
}
fn read_still(path: &Path) -> Result<CapturedImage> {
    let decoded = image::open(path)?.to_rgba8();
    let (width, height) = decoded.dimensions();
    let mut pixels = decoded.into_raw();
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Ok(CapturedImage {
        width,
        height,
        pixels,
    })
}

#[cfg(windows)]
struct FrameDecoder<'a> {
    session: &'a RecordedSession,
    runtime: Option<std::rc::Rc<media::MediaRuntime>>,
    reader: Option<media::VideoReader>,
    segment: Option<usize>,
    current: Option<(i64, CapturedImage)>,
    pending: Option<(i64, CapturedImage)>,
    ended: bool,
}
#[cfg(windows)]
impl<'a> FrameDecoder<'a> {
    fn new(session: &'a RecordedSession) -> Result<Self> {
        let runtime = if session.manifest.segments.is_empty() {
            None
        } else {
            Some(media::MediaRuntime::load()?)
        };
        Ok(Self {
            session,
            runtime,
            reader: None,
            segment: None,
            current: None,
            pending: None,
            ended: false,
        })
    }
    fn at(&mut self, time: i64) -> Result<CapturedImage> {
        let manifest = &self.session.manifest;
        if manifest.segments.is_empty() {
            let index = manifest
                .still_frames
                .partition_point(|frame| frame.time_100ns <= time)
                .saturating_sub(1);
            let frame = manifest
                .still_frames
                .get(index)
                .context("recording contains no video frames")?;
            return read_still(&self.session.directory.path.join(&frame.file));
        }
        let index = manifest
            .segments
            .partition_point(|segment| segment.start_100ns <= time)
            .saturating_sub(1);
        let segment = &manifest.segments[index];
        let relative = time - segment.start_100ns;
        if self.segment != Some(index)
            || self.current.as_ref().is_some_and(|(at, _)| *at > relative)
        {
            self.reader = Some(media::VideoReader::new(
                self.runtime
                    .as_ref()
                    .context("video decoder runtime missing")?
                    .clone(),
                &self.session.directory.path.join(&segment.file),
                manifest.width,
                manifest.height,
            )?);
            self.segment = Some(index);
            self.current = None;
            self.pending = None;
            self.ended = false;
        }
        loop {
            if self.pending.is_none() && !self.ended {
                self.pending = self
                    .reader
                    .as_ref()
                    .context("video reader missing")?
                    .next()?;
                self.ended = self.pending.is_none();
            }
            let accept = self
                .pending
                .as_ref()
                .is_some_and(|(at, _)| self.current.is_none() || *at <= relative);
            if accept {
                self.current = self.pending.take();
            } else {
                break;
            }
        }
        Ok(self
            .current
            .as_ref()
            .context("video segment has no decodable frames")?
            .1
            .clone())
    }
}
#[cfg(windows)]
pub fn read_frame(session: &RecordedSession, time: i64) -> Result<CapturedImage> {
    FrameDecoder::new(session)?.at(time)
}

/// Only a newly-created destination can be removed on error; user files are never overwritten.
struct ExportDestination {
    path: PathBuf,
    committed: bool,
}
impl Drop for ExportDestination {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.path);
        }
    }
}
pub(super) fn validate_destination(path: &Path, session: &Path) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    ensure!(
        !fs::canonicalize(parent)?.starts_with(fs::canonicalize(session)?),
        "export must be outside the temporary recording directory"
    );
    Ok(())
}

#[cfg(windows)]
pub fn export(
    session: &RecordedSession,
    path: &Path,
    format: ExportFormat,
    trim: TrimRange,
    options: &EffectOptions,
    cancel: &AtomicBool,
) -> Result<()> {
    let _com = media::ComApartment::new()?;
    validate_destination(path, &session.directory.path)?;
    let reservation = OpenOptions::new().create_new(true).write(true).open(path)?;
    let mut destination = ExportDestination {
        path: path.to_owned(),
        committed: false,
    };
    let duration = trim.end_100ns - trim.start_100ns;
    let manifest = &session.manifest;
    // Browsers lengthen <=10ms animation frames; sample both image formats at <=50fps.
    let fps = match format {
        ExportFormat::Mp4 => manifest.fps,
        ExportFormat::Gif | ExportFormat::WebP => manifest.fps.min(50),
    };
    let scaled = i128::from(duration) * i128::from(fps);
    let frames = if format == ExportFormat::Mp4 {
        (scaled + i128::from(TICKS_PER_SECOND) - 1) / i128::from(TICKS_PER_SECOND)
    } else {
        // Spread the trimmed remainder across frames instead of ending with a <=10ms tail.
        (scaled / i128::from(TICKS_PER_SECOND)).max(1)
    };
    let mut decoder = FrameDecoder::new(session)?;
    let mut mixer = audio::TrackMixer::open(
        &session.directory.path,
        options.system_audio_gain,
        options.microphone_gain,
    )?;
    let mut video = None;
    let mut gif = None;
    let mut webp = None;
    match format {
        ExportFormat::Mp4 => {
            drop(reservation);
            video = Some(media::VideoWriter::new(
                media::MediaRuntime::load()?,
                path,
                manifest.width,
                manifest.height,
                fps,
                manifest.bitrate,
                mixer.has_audio(),
            )?);
        }
        ExportFormat::Gif => {
            let mut encoder = image::codecs::gif::GifEncoder::new_with_speed(reservation, 10);
            encoder.set_repeat(image::codecs::gif::Repeat::Infinite)?;
            gif = Some(encoder);
        }
        ExportFormat::WebP => {
            webp = Some(AnimatedWebP::new(
                reservation,
                manifest.width,
                manifest.height,
            )?);
        }
    }
    for index in 0..frames as i64 {
        ensure!(
            !cancel.load(Ordering::Acquire),
            "recording export cancelled"
        );
        let (start, end) = if format == ExportFormat::Mp4 {
            (
                (i128::from(index) * i128::from(TICKS_PER_SECOND) / i128::from(fps)) as i64,
                ((i128::from(index + 1) * i128::from(TICKS_PER_SECOND) / i128::from(fps)) as i64)
                    .min(duration),
            )
        } else {
            (
                (i128::from(index) * i128::from(duration) / frames) as i64,
                (i128::from(index + 1) * i128::from(duration) / frames) as i64,
            )
        };
        let time = trim.start_100ns + start;
        let mut image = decoder.at(time)?;
        effects::compose(&mut image, &session.timeline.at(time, options), options)?;
        if let Some(writer) = &video {
            writer.frame(&image, start, end - start)?;
            if mixer.has_audio() {
                let first = (i128::from(time) * 48_000 / i128::from(TICKS_PER_SECOND)) as u64;
                let last = (i128::from(trim.start_100ns + end) * 48_000
                    / i128::from(TICKS_PER_SECOND)) as u64;
                let samples = mixer.read(first, (last - first) as usize)?;
                if !samples.is_empty() {
                    writer.audio(&samples, start)?;
                }
            }
        }
        if let Some(encoder) = &mut gif {
            let pixels = image::RgbaImage::from_raw(image.width, image.height, rgba(&image)?)
                .context("invalid GIF image dimensions")?;
            // Cumulative centisecond rounding avoids systematic 24fps timing drift.
            let delay =
                (((end + 50_000) / 100_000) - ((start + 50_000) / 100_000)).max(2) as u32 * 10;
            encoder.encode_frame(image::Frame::from_parts(
                pixels,
                0,
                0,
                image::Delay::from_numer_denom_ms(delay, 1),
            ))?;
        }
        if let Some(encoder) = &mut webp {
            let delay = (((end + 5_000) / 10_000) - ((start + 5_000) / 10_000)).max(20) as u32;
            encoder.frame(&image, delay)?;
        }
    }
    if let Some(writer) = video {
        writer.finish()?;
    }
    if let Some(encoder) = webp {
        encoder.finish()?;
    }
    drop(gif);
    OpenOptions::new().write(true).open(path)?.sync_all()?;
    destination.committed = true;
    Ok(())
}

/// WebP's small RIFF animation container wraps mature image-webp lossless frame encoding.
/// Frames are opaque full-canvas replacements, so disposal/blending cannot accumulate ghosts.
struct AnimatedWebP {
    file: File,
    width: u32,
    height: u32,
}
impl AnimatedWebP {
    fn new(mut file: File, width: u32, height: u32) -> Result<Self> {
        ensure!(
            width > 0 && height > 0 && width <= 16383 && height <= 16383,
            "WebP canvas exceeds codec limits"
        );
        file.write_all(b"RIFF\0\0\0\0WEBP")?;
        let mut extended = vec![0x02, 0, 0, 0];
        extended.extend_from_slice(&uint24(width - 1));
        extended.extend_from_slice(&uint24(height - 1));
        chunk(&mut file, b"VP8X", &extended)?;
        chunk(&mut file, b"ANIM", &[0, 0, 0, 255, 0, 0])?;
        Ok(Self {
            file,
            width,
            height,
        })
    }
    fn frame(&mut self, image: &CapturedImage, duration_ms: u32) -> Result<()> {
        ensure!(
            image.width == self.width && image.height == self.height && duration_ms <= 0xFF_FFFF,
            "invalid animated WebP frame"
        );
        let mut encoded = Vec::new();
        WebPEncoder::new_lossless(&mut encoded).encode(
            &rgba(image)?,
            image.width,
            image.height,
            ExtendedColorType::Rgba8,
        )?;
        ensure!(
            encoded.len() >= 20 && &encoded[..4] == b"RIFF" && &encoded[8..12] == b"WEBP",
            "WebP codec returned an invalid container"
        );
        let mut payload = vec![0; 6];
        payload.extend_from_slice(&uint24(self.width - 1));
        payload.extend_from_slice(&uint24(self.height - 1));
        payload.extend_from_slice(&uint24(duration_ms));
        payload.push(0x02); // No blending; the next full-canvas frame replaces this one.
        let mut position = 12;
        let mut found = false;
        while position + 8 <= encoded.len() {
            let size = u32::from_le_bytes(encoded[position + 4..position + 8].try_into()?) as usize;
            let end = position
                .checked_add(8 + size + (size & 1))
                .context("WebP chunk length overflow")?;
            ensure!(end <= encoded.len(), "truncated WebP frame chunk");
            let id = &encoded[position..position + 4];
            if id == b"VP8L" || id == b"VP8 " || id == b"ALPH" {
                payload.extend_from_slice(&encoded[position..end]);
                found |= id != b"ALPH";
            }
            position = end;
        }
        ensure!(found, "WebP encoder returned no image payload");
        chunk(&mut self.file, b"ANMF", &payload)?;
        Ok(())
    }
    fn finish(mut self) -> Result<()> {
        let length = self.file.stream_position()?;
        ensure!(
            length >= 12 && length - 8 <= u64::from(u32::MAX),
            "animated WebP exceeds RIFF 4GiB limit"
        );
        self.file.seek(SeekFrom::Start(4))?;
        self.file.write_all(&((length - 8) as u32).to_le_bytes())?;
        self.file.flush()?;
        Ok(())
    }
}
fn uint24(value: u32) -> [u8; 3] {
    [value as u8, (value >> 8) as u8, (value >> 16) as u8]
}
fn chunk(file: &mut File, name: &[u8; 4], payload: &[u8]) -> Result<()> {
    let length = u32::try_from(payload.len())?;
    file.write_all(name)?;
    file.write_all(&length.to_le_bytes())?;
    file.write_all(payload)?;
    if length & 1 != 0 {
        file.write_all(&[0])?;
    }
    Ok(())
}
