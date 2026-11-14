use super::{Segment, SessionManifest, StillFrame, TICKS_PER_SECOND, export, media};
use crate::CapturedImage;
use anyhow::{Context, Result, ensure};
use std::{
    path::{Path, PathBuf},
    rc::Rc,
};

/// Keep one captured image pending until its successor gives its exact QPC duration.
/// Unchanged WGC frames retain their timestamp; a static interval needs no raw-frame backlog.
pub(super) struct VideoTrack {
    writer: Option<media::VideoWriter>,
    runtime: Option<Rc<media::MediaRuntime>>,
    manifest: SessionManifest,
    directory: PathBuf,
    pending: Option<(i64, CapturedImage)>,
    last_source_time: Option<i64>,
    segment_start: i64,
    segment_index: u64,
    frame_index: u64,
}
impl VideoTrack {
    pub fn new(
        directory: &Path,
        manifest: SessionManifest,
        runtime: Option<Rc<media::MediaRuntime>>,
        writer: Option<media::VideoWriter>,
    ) -> Self {
        Self {
            writer,
            runtime,
            manifest,
            directory: directory.to_owned(),
            pending: None,
            last_source_time: None,
            segment_start: 0,
            segment_index: 0,
            frame_index: 0,
        }
    }
    pub fn has_frame(&self) -> bool {
        self.pending.is_some()
    }
    pub fn end_time(&self, requested: i64) -> i64 {
        let minimum = self.pending.as_ref().map_or(0, |(start, _)| {
            start
                + if self.manifest.quick {
                    TICKS_PER_SECOND / i64::from(self.manifest.fps)
                } else {
                    1
                }
        });
        requested.max(minimum)
    }
    pub fn audio(&self, samples: &[f32], time: i64) -> Result<()> {
        self.writer
            .as_ref()
            .context("quick recording encoder missing")?
            .audio(samples, time)
    }
    pub fn push(&mut self, source_time: i64, image: CapturedImage) -> Result<()> {
        if source_time < 0
            || self
                .last_source_time
                .is_some_and(|last| source_time <= last)
        {
            return Ok(());
        }
        ensure!(
            image.width == self.manifest.width && image.height == self.manifest.height,
            "capture dimensions changed during recording"
        );
        let time = if self.last_source_time.is_none() {
            0
        } else {
            source_time
        };
        self.last_source_time = Some(source_time);
        if self.manifest.quick {
            // Quick MP4 is variable-rate: unchanged content is held by the player, not
            // restamped at dequeue time. Audio packets can be muxed without a frame backlog.
            self.writer
                .as_ref()
                .context("quick encoder missing")?
                .frame(
                    &image,
                    time,
                    TICKS_PER_SECOND / i64::from(self.manifest.fps),
                )?;
        } else if let Some((start, previous)) = self.pending.take() {
            ensure!(
                time >= start,
                "capture frame arrived later than the recording timing buffer permits"
            );
            self.interval(&previous, start, time)?;
        }
        self.pending = Some((time, image));
        Ok(())
    }
    pub fn advance_static(&mut self, now: i64) -> Result<()> {
        if self.manifest.quick {
            return Ok(());
        }
        let period = TICKS_PER_SECOND / i64::from(self.manifest.fps);
        // Keep a watermark behind the three-slot capture pipeline. This bounds both
        // pending memory and stop-time work without stamping new images at dequeue time.
        let watermark = now - (period * 4).max(TICKS_PER_SECOND);
        if let Some((start, image)) = self.pending.take() {
            if watermark - start >= period {
                self.interval(&image, start, watermark)?;
                self.pending = Some((watermark, image));
            } else {
                self.pending = Some((start, image));
            }
        }
        Ok(())
    }
    fn interval(&mut self, image: &CapturedImage, mut start: i64, end: i64) -> Result<()> {
        while start < end {
            if self.runtime.is_some() && start == self.segment_start + TICKS_PER_SECOND {
                self.writer
                    .take()
                    .context("segment encoder missing")?
                    .finish()?;
                self.manifest.segments.push(Segment {
                    file: format!("video-{:06}.mp4", self.segment_index),
                    start_100ns: self.segment_start,
                    end_100ns: start,
                });
                self.segment_index += 1;
                self.segment_start = start;
                self.writer = Some(media::VideoWriter::new(
                    self.runtime.as_ref().context("MF runtime missing")?.clone(),
                    &self
                        .directory
                        .join(format!("video-{:06}.mp4", self.segment_index)),
                    self.manifest.width,
                    self.manifest.height,
                    self.manifest.fps,
                    self.manifest.bitrate,
                    false,
                )?);
            }
            let boundary = (start / TICKS_PER_SECOND + 1) * TICKS_PER_SECOND;
            let until = end.min(boundary);
            if let Some(writer) = &self.writer {
                writer.frame(image, start - self.segment_start, until - start)?;
            } else {
                let file = format!("frame-{:09}.webp", self.frame_index);
                export::write_still_webp(&self.directory.join(&file), image)?;
                self.manifest.still_frames.push(StillFrame {
                    file,
                    time_100ns: start,
                    duration_100ns: until - start,
                });
            }
            self.frame_index += 1;
            start = until;
        }
        Ok(())
    }
    pub fn finish(mut self, duration: i64) -> Result<SessionManifest> {
        let (start, image) = self
            .pending
            .take()
            .context("recording contains no captured frames")?;
        let duration = duration.max(start + 1);
        if self.manifest.quick {
            let period = TICKS_PER_SECOND / i64::from(self.manifest.fps);
            let last = (duration - period).max(start + period);
            if last < duration {
                self.writer
                    .as_ref()
                    .context("quick encoder missing")?
                    .frame(&image, last, duration - last)?;
            }
        } else {
            self.interval(&image, start, duration)?;
        }
        if let Some(writer) = self.writer.take() {
            writer.finish()?;
        }
        if self.runtime.is_some() {
            self.manifest.segments.push(Segment {
                file: if self.manifest.quick {
                    "quick.mp4".into()
                } else {
                    format!("video-{:06}.mp4", self.segment_index)
                },
                start_100ns: self.segment_start,
                end_100ns: duration,
            });
        }
        self.manifest.duration_100ns = duration;
        Ok(self.manifest)
    }
}
