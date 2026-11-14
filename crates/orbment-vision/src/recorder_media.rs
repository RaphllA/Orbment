use crate::CapturedImage;
use anyhow::{Context, Result, ensure};
use std::{ffi::c_void, path::Path, rc::Rc};
use windows::{
    Win32::{
        Foundation::{FreeLibrary, HMODULE},
        Media::MediaFoundation::*,
        System::{
            Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize},
            LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW},
        },
    },
    core::{GUID, HRESULT, Interface, PCSTR, PCWSTR},
};

type Symbol = unsafe extern "system" fn() -> isize;
type Startup = unsafe extern "system" fn(u32, u32) -> HRESULT;
type Shutdown = unsafe extern "system" fn() -> HRESULT;
type Create = unsafe extern "system" fn(*mut *mut c_void) -> HRESULT;
type CreateBuffer = unsafe extern "system" fn(u32, *mut *mut c_void) -> HRESULT;
type CreateAttributes = unsafe extern "system" fn(*mut *mut c_void, u32) -> HRESULT;
type CreateWriter =
    unsafe extern "system" fn(PCWSTR, *mut c_void, *mut c_void, *mut *mut c_void) -> HRESULT;
type CreateReader = unsafe extern "system" fn(PCWSTR, *mut c_void, *mut *mut c_void) -> HRESULT;

pub struct ComApartment;
impl ComApartment {
    pub fn new() -> Result<Self> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        }
        Ok(Self)
    }
}
impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

struct Library(HMODULE);
impl Library {
    fn open(name: &str) -> Result<Self> {
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        Ok(Self(
            unsafe { LoadLibraryExW(PCWSTR(wide.as_ptr()), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }
                .with_context(|| format!("load {name}; install the Windows Media Feature Pack"))?,
        ))
    }
    fn symbol(&self, name: &'static [u8]) -> Result<unsafe extern "system" fn() -> isize> {
        unsafe { GetProcAddress(self.0, PCSTR(name.as_ptr())) }
            .ok_or_else(|| anyhow::anyhow!("Media Foundation entrypoint unavailable"))
    }
}
impl Drop for Library {
    fn drop(&mut self) {
        unsafe {
            let _ = FreeLibrary(self.0);
        }
    }
}

/// Resolve MF lazily so Windows N/KN can still launch and use GIF/WebP recording.
pub struct MediaRuntime {
    shutdown: Option<Shutdown>,
    create_type: Create,
    create_sample: Create,
    create_buffer: CreateBuffer,
    create_attributes: CreateAttributes,
    create_writer: CreateWriter,
    create_reader: CreateReader,
    _platform: Library,
    _readwrite: Library,
}
impl MediaRuntime {
    pub fn load() -> Result<Rc<Self>> {
        let platform = Library::open("mfplat.dll")?;
        let readwrite = Library::open("mfreadwrite.dll")?;
        unsafe {
            let startup: Startup = std::mem::transmute(platform.symbol(b"MFStartup\0")?);
            let shutdown: Shutdown = std::mem::transmute(platform.symbol(b"MFShutdown\0")?);
            let mut runtime = Self {
                shutdown: None,
                create_type: std::mem::transmute::<Symbol, Create>(
                    platform.symbol(b"MFCreateMediaType\0")?,
                ),
                create_sample: std::mem::transmute::<Symbol, Create>(
                    platform.symbol(b"MFCreateSample\0")?,
                ),
                create_buffer: std::mem::transmute::<Symbol, CreateBuffer>(
                    platform.symbol(b"MFCreateMemoryBuffer\0")?,
                ),
                create_attributes: std::mem::transmute::<Symbol, CreateAttributes>(
                    platform.symbol(b"MFCreateAttributes\0")?,
                ),
                create_writer: std::mem::transmute::<Symbol, CreateWriter>(
                    readwrite.symbol(b"MFCreateSinkWriterFromURL\0")?,
                ),
                create_reader: std::mem::transmute::<Symbol, CreateReader>(
                    readwrite.symbol(b"MFCreateSourceReaderFromURL\0")?,
                ),
                _platform: platform,
                _readwrite: readwrite,
            };
            startup(MF_VERSION, MFSTARTUP_FULL).ok()?;
            runtime.shutdown = Some(shutdown);
            Ok(Rc::new(runtime))
        }
    }
    fn media_type(&self) -> Result<IMFMediaType> {
        unsafe {
            let mut raw = std::ptr::null_mut();
            (self.create_type)(&mut raw).ok()?;
            Ok(IMFMediaType::from_raw(raw))
        }
    }
    fn attributes(&self) -> Result<IMFAttributes> {
        unsafe {
            let mut raw = std::ptr::null_mut();
            (self.create_attributes)(&mut raw, 4).ok()?;
            Ok(IMFAttributes::from_raw(raw))
        }
    }
    fn sample(&self, bytes: &[u8], time: i64, duration: i64) -> Result<IMFSample> {
        unsafe {
            let mut raw = std::ptr::null_mut();
            (self.create_sample)(&mut raw).ok()?;
            let sample = IMFSample::from_raw(raw);
            (self.create_buffer)(bytes.len().try_into()?, &mut raw).ok()?;
            let buffer = IMFMediaBuffer::from_raw(raw);
            let mut target = std::ptr::null_mut();
            buffer.Lock(&mut target, None, None)?;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), target, bytes.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(bytes.len().try_into()?)?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(time)?;
            sample.SetSampleDuration(duration)?;
            Ok(sample)
        }
    }
}
impl Drop for MediaRuntime {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown {
            unsafe {
                let _ = shutdown();
            }
        }
    }
}

pub struct VideoWriter {
    writer: IMFSinkWriter,
    video: u32,
    audio: Option<u32>,
    width: u32,
    height: u32,
    runtime: Rc<MediaRuntime>,
}
impl VideoWriter {
    pub fn new(
        runtime: Rc<MediaRuntime>,
        path: &Path,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        audio: bool,
    ) -> Result<Self> {
        ensure!(
            width.is_multiple_of(2) && height.is_multiple_of(2),
            "H.264 dimensions must be even"
        );
        let url: Vec<u16> = path
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain(Some(0))
            .collect();
        unsafe {
            let attrs = runtime.attributes()?;
            attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
            attrs.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
            let mut raw = std::ptr::null_mut();
            (runtime.create_writer)(
                PCWSTR(url.as_ptr()),
                std::ptr::null_mut(),
                attrs.as_raw(),
                &mut raw,
            )
            .ok()?;
            let writer = IMFSinkWriter::from_raw(raw);
            let output = runtime.media_type()?;
            set_video(&output, &MFVideoFormat_H264, width, height, fps)?;
            output.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            output.SetUINT32(&MF_MT_MAX_KEYFRAME_SPACING, fps)?;
            let video = writer.AddStream(&output)?;
            let input = runtime.media_type()?;
            set_video(&input, &MFVideoFormat_RGB32, width, height, fps)?;
            input.SetUINT32(&MF_MT_DEFAULT_STRIDE, width * 4)?;
            writer.SetInputMediaType(video, &input, None)?;
            let audio = if audio {
                let output = runtime.media_type()?;
                set_audio(&output, &MFAudioFormat_AAC)?;
                output.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 24_000)?;
                output.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?;
                output.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)?;
                let stream = writer.AddStream(&output)?;
                let input = runtime.media_type()?;
                set_audio(&input, &MFAudioFormat_PCM)?;
                input.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)?;
                input.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 192_000)?;
                writer.SetInputMediaType(stream, &input, None)?;
                Some(stream)
            } else {
                None
            };
            writer.BeginWriting()?;
            let mut codec = std::ptr::null_mut();
            writer.GetServiceForStream(video, &GUID::zeroed(), &ICodecAPI::IID, &mut codec)?;
            let codec = ICodecAPI::from_raw(codec);
            let gop = windows::Win32::System::Variant::VARIANT::from(fps);
            codec
                .SetValue(&CODECAPI_AVEncMPVGOPSize, &gop)
                .context("configure H.264 keyframes at most one second apart")?;
            Ok(Self {
                writer,
                video,
                audio,
                width,
                height,
                runtime,
            })
        }
    }
    pub fn frame(&self, image: &CapturedImage, time: i64, duration: i64) -> Result<()> {
        ensure!(
            image.width == self.width
                && image.height == self.height
                && image.pixels.len() == self.width as usize * self.height as usize * 4,
            "encoder frame dimensions changed"
        );
        // MF RGB32 uses a positive bottom-up stride; captures are top-down BGRA.
        let row = self.width as usize * 4;
        let mut pixels = Vec::with_capacity(image.pixels.len());
        for y in (0..self.height as usize).rev() {
            pixels.extend_from_slice(&image.pixels[y * row..(y + 1) * row]);
        }
        let sample = self.runtime.sample(&pixels, time, duration)?;
        unsafe {
            self.writer.WriteSample(self.video, &sample)?;
        }
        Ok(())
    }
    pub fn audio(&self, samples: &[f32], time: i64) -> Result<()> {
        let Some(stream) = self.audio else {
            return Ok(());
        };
        let bytes: Vec<u8> = samples
            .iter()
            .flat_map(|v| ((v.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes())
            .collect();
        let duration = samples.len() as i64 / 2 * 10_000_000 / 48_000;
        let sample = self.runtime.sample(&bytes, time, duration)?;
        unsafe {
            self.writer.WriteSample(stream, &sample)?;
        }
        Ok(())
    }
    pub fn finish(self) -> Result<()> {
        unsafe {
            self.writer.Finalize()?;
        }
        Ok(())
    }
}

unsafe fn set_video(
    media: &IMFMediaType,
    subtype: &GUID,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<()> {
    unsafe {
        media.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        media.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        media.SetUINT64(
            &MF_MT_FRAME_SIZE,
            (u64::from(width) << 32) | u64::from(height),
        )?;
        media.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(fps) << 32) | 1)?;
        media.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
    }
    Ok(())
}
unsafe fn set_audio(media: &IMFMediaType, subtype: &GUID) -> Result<()> {
    unsafe {
        media.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        media.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        media.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
        media.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, 48_000)?;
        media.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
    }
    Ok(())
}

pub struct VideoReader {
    reader: IMFSourceReader,
    width: u32,
    height: u32,
    _runtime: Rc<MediaRuntime>,
}
impl VideoReader {
    pub fn new(runtime: Rc<MediaRuntime>, path: &Path, width: u32, height: u32) -> Result<Self> {
        let url: Vec<u16> = path
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain(Some(0))
            .collect();
        unsafe {
            let attrs = runtime.attributes()?;
            attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)?;
            let mut raw = std::ptr::null_mut();
            (runtime.create_reader)(PCWSTR(url.as_ptr()), attrs.as_raw(), &mut raw).ok()?;
            let reader = IMFSourceReader::from_raw(raw);
            reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
            reader.SetStreamSelection(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, true)?;
            let media = runtime.media_type()?;
            media.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            media.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
            reader.SetCurrentMediaType(
                MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                None,
                &media,
            )?;
            Ok(Self {
                reader,
                width,
                height,
                _runtime: runtime,
            })
        }
    }
    pub fn next(&self) -> Result<Option<(i64, CapturedImage)>> {
        unsafe {
            loop {
                let mut flags = 0;
                let mut time = 0;
                let mut sample = None;
                self.reader.ReadSample(
                    MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut time),
                    Some(&mut sample),
                )?;
                if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                    return Ok(None);
                }
                if let Some(sample) = sample {
                    let buffer = sample.ConvertToContiguousBuffer()?;
                    let mut pixels = vec![0; self.width as usize * self.height as usize * 4];
                    if let Ok(buffer2d) = buffer.cast::<IMF2DBuffer>() {
                        let mut scan = std::ptr::null_mut();
                        let mut pitch = 0;
                        buffer2d.Lock2D(&mut scan, &mut pitch)?;
                        for y in 0..self.height as usize {
                            let row = std::slice::from_raw_parts(
                                scan.offset(y as isize * pitch as isize),
                                self.width as usize * 4,
                            );
                            pixels[y * self.width as usize * 4..(y + 1) * self.width as usize * 4]
                                .copy_from_slice(row);
                        }
                        buffer2d.Unlock2D()?;
                    } else {
                        let media = self
                            .reader
                            .GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32)?;
                        let stride = media
                            .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                            .unwrap_or(self.width * 4) as i32;
                        let mut data = std::ptr::null_mut();
                        let mut len = 0;
                        buffer.Lock(&mut data, None, Some(&mut len))?;
                        let required = stride.unsigned_abs() as usize * self.height as usize;
                        if required > len as usize {
                            buffer.Unlock()?;
                            anyhow::bail!("decoded video buffer is shorter than its stride");
                        }
                        for y in 0..self.height as usize {
                            let source_y = if stride >= 0 {
                                self.height as usize - 1 - y
                            } else {
                                y
                            };
                            let row = std::slice::from_raw_parts(
                                data.add(source_y * stride.unsigned_abs() as usize),
                                self.width as usize * 4,
                            );
                            pixels[y * self.width as usize * 4..(y + 1) * self.width as usize * 4]
                                .copy_from_slice(row);
                        }
                        buffer.Unlock()?;
                    }
                    for pixel in pixels.as_chunks_mut::<4>().0 {
                        pixel[3] = 255;
                    }
                    return Ok(Some((
                        time,
                        CapturedImage {
                            width: self.width,
                            height: self.height,
                            pixels,
                        },
                    )));
                }
            }
        }
    }
}
