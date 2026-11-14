//! The high-level `ort` crate pins its API/library in process-global OnceLocks.
//! Use its generated C ABI (`ort-sys`, disable-linking) so one worker owns every
//! session, environment and DLL reference and can actually unload them at idle.
use anyhow::{Context, Result, ensure};
use image::{
    RgbImage,
    imageops::{self, FilterType},
};
use orbment_core::{
    config::VisionOcrConfig,
    ocr::{OcrBoundingBox, OcrEngine, OcrLine, OcrResult, OcrWord},
};
use ort_sys::*;
use std::{
    ffi::{CStr, CString},
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    ptr,
    sync::{
        Mutex,
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use windows::{
    Win32::{
        Foundation::{FreeLibrary, HMODULE},
        System::LibraryLoader::{
            GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32,
            LoadLibraryExW,
        },
    },
    core::{PCSTR, PCWSTR},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Punctuation {
    #[default]
    Auto,
    Chinese,
    English,
}

#[derive(Clone, Debug, Default)]
pub struct PostprocessOptions {
    pub remove_spaces: bool,
    pub punctuation: Punctuation,
    pub vertical_reading: bool,
}
impl From<&VisionOcrConfig> for PostprocessOptions {
    fn from(config: &VisionOcrConfig) -> Self {
        Self {
            remove_spaces: config.remove_spaces,
            punctuation: match config.punctuation_type.as_str() {
                "Chinese" => Punctuation::Chinese,
                "English" => Punctuation::English,
                _ => Punctuation::Auto,
            },
            vertical_reading: config.vertical_reading,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ModelFiles {
    pub runtime: PathBuf,
    pub detector: PathBuf,
    pub recognizer: PathBuf,
    pub dictionary: PathBuf,
}
impl ModelFiles {
    pub fn in_data_dir(data_dir: &Path) -> Self {
        let models = data_dir.join("models");
        Self {
            runtime: models.join("onnxruntime.dll"),
            detector: models.join("ch_PP-OCRv4_det_infer.onnx"),
            recognizer: models.join("ch_PP-OCRv4_rec_infer.onnx"),
            dictionary: models.join("ppocr_keys_v1.txt"),
        }
    }
}

pub struct OnnxOcr {
    files: ModelFiles,
    idle: Duration,
    options: PostprocessOptions,
    worker: Mutex<Option<Worker>>,
}
struct Worker {
    sender: Sender<Request>,
    thread: JoinHandle<()>,
}
enum Request {
    Recognize(PathBuf, Sender<Result<OcrResult>>),
    Stop,
}

impl OnnxOcr {
    /// Inert: no thread, file reads, DLL imports or model sessions at startup.
    pub fn new(data_dir: &Path, idle: Duration) -> Self {
        Self {
            files: ModelFiles::in_data_dir(data_dir),
            idle,
            options: PostprocessOptions::default(),
            worker: Mutex::new(None),
        }
    }
    pub fn with_options(mut self, options: PostprocessOptions) -> Self {
        self.options = options;
        self
    }
    pub fn with_models(mut self, files: ModelFiles) -> Self {
        self.files = files;
        self
    }
    pub fn model_files(&self) -> &ModelFiles {
        &self.files
    }
}
impl OcrEngine for OnnxOcr {
    fn is_ready(&self) -> bool {
        [
            &self.files.runtime,
            &self.files.detector,
            &self.files.recognizer,
            &self.files.dictionary,
        ]
        .into_iter()
        .all(|path| path.is_file())
    }

    fn recognize(&self, image_path: &Path) -> Result<OcrResult> {
        let (reply, receive) = mpsc::channel();
        {
            let mut worker = self
                .worker
                .lock()
                .map_err(|_| anyhow::anyhow!("OCR worker lock poisoned"))?;
            if worker.is_none() {
                let (sender, receiver) = mpsc::channel();
                let files = self.files.clone();
                let options = self.options.clone();
                let idle = self.idle;
                let thread = thread::Builder::new()
                    .name("orbment-onnx-ocr".into())
                    .spawn(move || worker_loop(receiver, files, options, idle))?;
                *worker = Some(Worker { sender, thread });
            }
            worker
                .as_ref()
                .expect("worker created")
                .sender
                .send(Request::Recognize(image_path.to_owned(), reply))
                .context("OCR worker exited")?;
        }
        receive
            .recv()
            .context("OCR worker ended before returning recognition")?
    }
}
impl Drop for OnnxOcr {
    fn drop(&mut self) {
        if let Some(worker) = self
            .worker
            .get_mut()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
        {
            let _ = worker.sender.send(Request::Stop);
            let _ = worker.thread.join();
        }
    }
}
fn worker_loop(
    receiver: Receiver<Request>,
    files: ModelFiles,
    options: PostprocessOptions,
    idle: Duration,
) {
    let mut runtime: Option<Runtime> = None;
    loop {
        let request = if runtime.is_some() {
            match receiver.recv_timeout(idle) {
                Ok(request) => request,
                Err(RecvTimeoutError::Timeout) => {
                    runtime = None;
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match receiver.recv() {
                Ok(request) => request,
                Err(_) => break,
            }
        };
        match request {
            Request::Stop => break,
            Request::Recognize(path, reply) => {
                let result = (|| {
                    if runtime.is_none() {
                        runtime = Some(Runtime::load(&files)?);
                    }
                    let mut reader = image::ImageReader::open(&path)
                        .with_context(|| format!("Open OCR image: {}", path.display()))?
                        .with_guessed_format()?;
                    let mut limits = image::Limits::default();
                    limits.max_alloc = Some(512 * 1024 * 1024);
                    limits.max_image_width = Some(32768);
                    limits.max_image_height = Some(32768);
                    reader.limits(limits);
                    let image = reader.decode().context("Decode OCR image")?.to_rgb8();
                    runtime
                        .as_ref()
                        .expect("runtime loaded")
                        .recognize(&image, &options)
                })();
                let _ = reply.send(result);
                // Zero means release immediately, including when more work is queued.
                if idle.is_zero() {
                    runtime = None;
                }
            }
        }
    }
}

struct Library(HMODULE);
impl Drop for Library {
    fn drop(&mut self) {
        unsafe {
            let _ = FreeLibrary(self.0);
        }
    }
}
struct Owned<T> {
    ptr: *mut T,
    release: unsafe extern "system" fn(*mut T),
}
impl<T> Owned<T> {
    fn empty(release: unsafe extern "system" fn(*mut T)) -> Self {
        Self {
            ptr: ptr::null_mut(),
            release,
        }
    }
}
impl<T> Drop for Owned<T> {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe {
                (self.release)(self.ptr);
            }
        }
    }
}
struct Model {
    session: Owned<OrtSession>,
    input: CString,
    output: CString,
    shape: Vec<i64>,
}
struct Runtime {
    // Field order is intentional: drop sessions before environment, DLL last.
    detector: Model,
    recognizer: Model,
    memory: Owned<OrtMemoryInfo>,
    _environment: Owned<OrtEnv>,
    dictionary: Vec<String>,
    api: *const OrtApi,
    _library: Library,
}
impl Runtime {
    fn load(files: &ModelFiles) -> Result<Self> {
        for path in [
            &files.runtime,
            &files.detector,
            &files.recognizer,
            &files.dictionary,
        ] {
            ensure!(
                path.is_file(),
                "Offline OCR file is missing: {}",
                path.display()
            );
        }
        let dictionary_text =
            std::fs::read_to_string(&files.dictionary).context("Read OCR dictionary as UTF-8")?;
        let mut dictionary = vec![String::new()]; // CTC blank is always class zero.
        dictionary.extend(
            dictionary_text
                .trim_start_matches('\u{feff}')
                .lines()
                .map(|line| line.trim_end_matches('\r').to_owned()),
        );
        ensure!(
            dictionary.len() > 1 && dictionary.iter().skip(1).all(|key| !key.is_empty()),
            "OCR dictionary contains empty symbols"
        );
        if dictionary.last().is_none_or(|key| key != " ") {
            dictionary.push(" ".into());
        }
        let library_path = files
            .runtime
            .canonicalize()
            .context("Resolve ONNX runtime path")?;
        let wide: Vec<u16> = library_path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        unsafe {
            let library = Library(
                LoadLibraryExW(
                    PCWSTR(wide.as_ptr()),
                    None,
                    LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
                )
                .with_context(|| format!("Load ONNX runtime: {}", library_path.display()))?,
            );
            let symbol = GetProcAddress(library.0, PCSTR(c"OrtGetApiBase".as_ptr().cast()))
                .context("onnxruntime.dll does not export OrtGetApiBase")?;
            let get_api_base: unsafe extern "system" fn() -> *const OrtApiBase =
                std::mem::transmute(symbol);
            let base = get_api_base();
            ensure!(!base.is_null(), "OrtGetApiBase returned null");
            let api_pointer = ((*base).GetApi)(ORT_API_VERSION);
            ensure!(
                !api_pointer.is_null(),
                "ONNX Runtime 1.22 or newer (C API {}) is required",
                ORT_API_VERSION
            );
            let api = &*api_pointer;
            let mut environment = Owned::empty(api.ReleaseEnv);
            status(
                api,
                (api.CreateEnv)(
                    OrtLoggingLevel::ORT_LOGGING_LEVEL_WARNING,
                    c"OrbmentOCR".as_ptr(),
                    &mut environment.ptr,
                ),
            )?;
            status(api, (api.DisableTelemetryEvents)(environment.ptr))?;
            let mut options = Owned::empty(api.ReleaseSessionOptions);
            status(api, (api.CreateSessionOptions)(&mut options.ptr))?;
            status(api, (api.SetIntraOpNumThreads)(options.ptr, 2))?;
            status(api, (api.SetInterOpNumThreads)(options.ptr, 1))?;
            status(
                api,
                (api.SetSessionGraphOptimizationLevel)(
                    options.ptr,
                    GraphOptimizationLevel::ORT_ENABLE_ALL,
                ),
            )?;
            let detector = Model::load(api, environment.ptr, options.ptr, &files.detector)?;
            let recognizer = Model::load(api, environment.ptr, options.ptr, &files.recognizer)?;
            let mut memory = Owned::empty(api.ReleaseMemoryInfo);
            status(
                api,
                (api.CreateCpuMemoryInfo)(
                    OrtAllocatorType::OrtArenaAllocator,
                    OrtMemType::OrtMemTypeDefault,
                    &mut memory.ptr,
                ),
            )?;
            Ok(Self {
                detector,
                recognizer,
                memory,
                _environment: environment,
                dictionary,
                api: api_pointer,
                _library: library,
            })
        }
    }

    fn recognize(&self, image: &RgbImage, options: &PostprocessOptions) -> Result<OcrResult> {
        ensure!(image.width() > 0 && image.height() > 0, "Empty OCR image");
        let ratio = (960.0 / image.width().max(image.height()) as f64).min(1.0);
        let dynamic_w = ((image.width() as f64 * ratio / 32.0).round() as u32).max(1) * 32;
        let dynamic_h = ((image.height() as f64 * ratio / 32.0).round() as u32).max(1) * 32;
        let width = fixed_dimension(&self.detector.shape, 3, dynamic_w)?;
        let height = fixed_dimension(&self.detector.shape, 2, dynamic_h)?;
        let resized = imageops::resize(image, width, height, FilterType::Triangle);
        let mut input = chw(&resized, false);
        let (shape, probabilities) = self.run(
            &self.detector,
            &mut input,
            &[1, 3, height as i64, width as i64],
        )?;
        ensure!(
            shape.len() == 4 && shape[0] == 1 && shape[1] == 1,
            "DB detector must return [1,1,H,W], got {shape:?}"
        );
        let map_h = usize::try_from(shape[2])?;
        let map_w = usize::try_from(shape[3])?;
        ensure!(
            map_w > 0 && map_h > 0 && map_w.checked_mul(map_h) == Some(probabilities.len()),
            "Invalid detector probability map"
        );
        let mut boxes = detect_boxes(&probabilities, map_w, map_h, image.width(), image.height());
        sort_boxes(&mut boxes, options.vertical_reading);
        let mut lines = Vec::new();
        for bounds in boxes {
            let vertical = bounds.height > bounds.width * 1.5;
            let x = bounds.x.max(0.0).floor() as u32;
            let y = bounds.y.max(0.0).floor() as u32;
            let crop_w = (bounds.width.ceil() as u32).min(image.width() - x).max(1);
            let crop_h = (bounds.height.ceil() as u32).min(image.height() - y).max(1);
            let crop = imageops::crop_imm(image, x, y, crop_w, crop_h).to_image();
            let crop = if vertical {
                imageops::rotate270(&crop)
            } else {
                crop
            };
            let rec_h = fixed_dimension(&self.recognizer.shape, 2, 48)?;
            let natural_w = (crop.width() as f64 * rec_h as f64 / crop.height() as f64)
                .ceil()
                .max(1.0) as u32;
            let rec_w = fixed_dimension(&self.recognizer.shape, 3, natural_w.clamp(32, 4096))?;
            let content_w = natural_w.min(rec_w);
            let resized = imageops::resize(&crop, content_w, rec_h, FilterType::Triangle);
            let mut padded = RgbImage::from_pixel(rec_w, rec_h, image::Rgb([127, 127, 127]));
            imageops::replace(&mut padded, &resized, 0, 0);
            let mut input = chw(&padded, true);
            let (shape, scores) = self.run(
                &self.recognizer,
                &mut input,
                &[1, 3, rec_h as i64, rec_w as i64],
            )?;
            ensure!(
                shape.len() == 3 && shape[0] == 1,
                "CTC recognizer must return [1,T,C], got {shape:?}"
            );
            let steps = usize::try_from(shape[1])?;
            let classes = usize::try_from(shape[2])?;
            ensure!(
                classes == self.dictionary.len(),
                "OCR dictionary/model mismatch: model has {classes} classes, dictionary has {} (including blank and space)",
                self.dictionary.len()
            );
            ensure!(
                steps > 0 && scores.len() == steps * classes,
                "Invalid recognizer output"
            );
            let valid_steps =
                ((steps as f64 * content_w as f64 / rec_w as f64).ceil() as usize).min(steps);
            let mut tokens: Vec<Token> = Vec::new();
            let mut previous = 0;
            let mut run_start = 0;
            let mut emitted = false;
            for t in 0..valid_steps {
                let row = &scores[t * classes..(t + 1) * classes];
                ensure!(
                    row.iter().all(|value| value.is_finite()),
                    "Non-finite OCR recognizer output"
                );
                let (class, confidence) = row
                    .iter()
                    .enumerate()
                    .max_by(|(_, a), (_, b)| a.total_cmp(b))
                    .expect("nonempty vocabulary");
                if class != previous {
                    run_start = t;
                    emitted = false;
                }
                if class != 0 {
                    if emitted {
                        if let Some(last) = tokens.last_mut() {
                            last.end = t + 1;
                        }
                    } else if *confidence >= 0.2 {
                        tokens.push(Token {
                            text: self.dictionary[class].clone(),
                            start: run_start,
                            end: t + 1,
                        });
                        emitted = true;
                    }
                }
                previous = class;
            }
            normalize_tokens(&mut tokens, options);
            if tokens.is_empty() {
                continue;
            }
            let mut words = Vec::new();
            for (index, token) in tokens.iter().enumerate() {
                // CTC peaks delimit glyph centers. Midpoints between neighboring
                // peaks estimate glyph extents without equally dividing the line.
                let start = if index == 0 {
                    0.0
                } else {
                    (tokens[index - 1].end + token.start) as f64 * 0.5 / valid_steps as f64
                };
                let end = if index + 1 == tokens.len() {
                    1.0
                } else {
                    (token.end + tokens[index + 1].start) as f64 * 0.5 / valid_steps as f64
                };
                let bounding_box = if vertical {
                    OcrBoundingBox {
                        x: bounds.x,
                        y: bounds.y + bounds.height * start as f32,
                        width: bounds.width,
                        height: bounds.height * (end - start) as f32,
                    }
                } else {
                    OcrBoundingBox {
                        x: bounds.x + bounds.width * start as f32,
                        y: bounds.y,
                        width: bounds.width * (end - start) as f32,
                        height: bounds.height,
                    }
                };
                words.push(OcrWord {
                    text: token.text.clone(),
                    bounding_box,
                });
            }
            let text = tokens
                .iter()
                .map(|token| token.text.as_str())
                .collect::<String>();
            lines.push(OcrLine {
                text,
                bounding_box: bounds,
                words,
            });
        }
        Ok(OcrResult {
            text: lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            lines,
        })
    }

    fn run(&self, model: &Model, data: &mut [f32], shape: &[i64]) -> Result<(Vec<i64>, Vec<f32>)> {
        unsafe {
            let api = &*self.api;
            let mut input = Owned::empty(api.ReleaseValue);
            status(
                api,
                (api.CreateTensorWithDataAsOrtValue)(
                    self.memory.ptr,
                    data.as_mut_ptr().cast(),
                    std::mem::size_of_val(data),
                    shape.as_ptr(),
                    shape.len(),
                    ONNXTensorElementDataType::ONNX_TENSOR_ELEMENT_DATA_TYPE_FLOAT,
                    &mut input.ptr,
                ),
            )?;
            let mut output = Owned::empty(api.ReleaseValue);
            status(
                api,
                (api.Run)(
                    model.session.ptr,
                    ptr::null(),
                    [model.input.as_ptr()].as_ptr(),
                    [input.ptr as *const OrtValue].as_ptr(),
                    1,
                    [model.output.as_ptr()].as_ptr(),
                    1,
                    &mut output.ptr,
                ),
            )
            .context("ONNX inference failed")?;
            let mut info = Owned::empty(api.ReleaseTensorTypeAndShapeInfo);
            status(api, (api.GetTensorTypeAndShape)(output.ptr, &mut info.ptr))?;
            let shape = tensor_shape(api, info.ptr)?;
            let mut count = 0;
            status(api, (api.GetTensorShapeElementCount)(info.ptr, &mut count))?;
            ensure!(
                count <= 256 * 1024 * 1024 && shape.iter().all(|&d| d > 0),
                "OCR output tensor exceeds limits or has invalid dimensions"
            );
            let mut pointer = ptr::null_mut();
            status(api, (api.GetTensorMutableData)(output.ptr, &mut pointer))?;
            ensure!(!pointer.is_null(), "ONNX output has null storage");
            Ok((
                shape,
                std::slice::from_raw_parts(pointer.cast::<f32>(), count).to_vec(),
            ))
        }
    }
}
impl Model {
    unsafe fn load(
        api: &OrtApi,
        env: *const OrtEnv,
        options: *const OrtSessionOptions,
        path: &Path,
    ) -> Result<Self> {
        unsafe {
            let mut session = Owned::empty(api.ReleaseSession);
            let path_wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            status(
                api,
                (api.CreateSession)(env, path_wide.as_ptr(), options, &mut session.ptr),
            )
            .with_context(|| format!("Load OCR model: {}", path.display()))?;
            let mut input_count = 0;
            let mut output_count = 0;
            status(
                api,
                (api.SessionGetInputCount)(session.ptr, &mut input_count),
            )?;
            status(
                api,
                (api.SessionGetOutputCount)(session.ptr, &mut output_count),
            )?;
            ensure!(
                input_count == 1 && output_count == 1,
                "OCR models must have exactly one input and one output"
            );
            let mut allocator = ptr::null_mut();
            status(api, (api.GetAllocatorWithDefaultOptions)(&mut allocator))?;
            let mut name = ptr::null_mut();
            status(
                api,
                (api.SessionGetInputName)(session.ptr, 0, allocator, &mut name),
            )?;
            ensure!(!name.is_null(), "OCR model input has no name");
            let input = CStr::from_ptr(name).to_owned();
            status(api, (api.AllocatorFree)(allocator, name.cast()))?;
            status(
                api,
                (api.SessionGetOutputName)(session.ptr, 0, allocator, &mut name),
            )?;
            ensure!(!name.is_null(), "OCR model output has no name");
            let output = CStr::from_ptr(name).to_owned();
            status(api, (api.AllocatorFree)(allocator, name.cast()))?;
            let mut info = Owned::empty(api.ReleaseTypeInfo);
            status(
                api,
                (api.SessionGetInputTypeInfo)(session.ptr, 0, &mut info.ptr),
            )?;
            let mut tensor_info = ptr::null();
            status(
                api,
                (api.CastTypeInfoToTensorInfo)(info.ptr, &mut tensor_info),
            )?;
            ensure!(!tensor_info.is_null(), "OCR input is not a tensor");
            let shape = tensor_shape(api, tensor_info)?;
            ensure!(
                shape.len() == 4 && (shape[0] == 1 || shape[0] == -1) && shape[1] == 3,
                "OCR input must be RGB NCHW, got {shape:?}"
            );
            Ok(Self {
                session,
                input,
                output,
                shape,
            })
        }
    }
}
fn status(api: &OrtApi, result: OrtStatusPtr) -> Result<()> {
    if result.0.is_null() {
        return Ok(());
    }
    unsafe {
        let message = CStr::from_ptr((api.GetErrorMessage)(result.0))
            .to_string_lossy()
            .into_owned();
        (api.ReleaseStatus)(result.0);
        anyhow::bail!("ONNX Runtime: {message}")
    }
}
unsafe fn tensor_shape(api: &OrtApi, info: *const OrtTensorTypeAndShapeInfo) -> Result<Vec<i64>> {
    unsafe {
        let mut kind = ONNXTensorElementDataType::ONNX_TENSOR_ELEMENT_DATA_TYPE_UNDEFINED;
        status(api, (api.GetTensorElementType)(info, &mut kind))?;
        ensure!(
            kind == ONNXTensorElementDataType::ONNX_TENSOR_ELEMENT_DATA_TYPE_FLOAT,
            "OCR tensor must be float32"
        );
        let mut dimensions = 0;
        status(api, (api.GetDimensionsCount)(info, &mut dimensions))?;
        ensure!(
            (1..=8).contains(&dimensions),
            "Invalid OCR tensor rank: {dimensions}"
        );
        let mut shape = vec![0; dimensions];
        status(
            api,
            (api.GetDimensions)(info, shape.as_mut_ptr(), dimensions),
        )?;
        Ok(shape)
    }
}
fn fixed_dimension(shape: &[i64], index: usize, dynamic: u32) -> Result<u32> {
    let value = if shape[index] > 0 {
        u32::try_from(shape[index])?
    } else {
        dynamic
    };
    ensure!(
        (1..=8192).contains(&value),
        "Unsupported OCR input dimension: {value}"
    );
    Ok(value)
}
fn chw(image: &RgbImage, recognition: bool) -> Vec<f32> {
    let area = image.width() as usize * image.height() as usize;
    let mut data = vec![0.0; area * 3];
    let mean = [0.485, 0.456, 0.406];
    let std = [0.229, 0.224, 0.225];
    for (index, pixel) in image.pixels().enumerate() {
        for channel in 0..3 {
            let value = pixel[channel] as f32 / 255.0;
            data[channel * area + index] = if recognition {
                (value - 0.5) * 2.0
            } else {
                (value - mean[channel]) / std[channel]
            };
        }
    }
    data
}

/// DB segmentation: connected foreground contours, probability scoring and
/// area/perimeter unclipping, mapped back to the original physical image pixels.
fn detect_boxes(
    map: &[f32],
    width: usize,
    height: usize,
    image_w: u32,
    image_h: u32,
) -> Vec<OcrBoundingBox> {
    let mut visited = vec![false; map.len()];
    let mut boxes = Vec::new();
    for seed in 0..map.len() {
        if visited[seed] || map[seed] < 0.3 || !map[seed].is_finite() {
            continue;
        }
        visited[seed] = true;
        let mut queue = vec![seed];
        let mut position = 0;
        let (mut left, mut right, mut top, mut bottom) = (width, 0, height, 0);
        let mut sum = 0.0;
        while position < queue.len() {
            let index = queue[position];
            position += 1;
            let x = index % width;
            let y = index / width;
            left = left.min(x);
            right = right.max(x);
            top = top.min(y);
            bottom = bottom.max(y);
            sum += map[index] as f64;
            for ny in y.saturating_sub(1)..=(y + 1).min(height - 1) {
                for nx in x.saturating_sub(1)..=(x + 1).min(width - 1) {
                    let next = ny * width + nx;
                    if !visited[next] && map[next].is_finite() && map[next] >= 0.3 {
                        visited[next] = true;
                        queue.push(next);
                    }
                }
            }
        }
        if queue.len() < 4 || sum / (queue.len() as f64) < 0.5 {
            continue;
        }
        let w = (right - left + 1) as f64;
        let h = (bottom - top + 1) as f64;
        if w.min(h) < 2.0 {
            continue;
        }
        let expansion = w * h * 1.5 / (2.0 * (w + h));
        let scale_x = image_w as f64 / width as f64;
        let scale_y = image_h as f64 / height as f64;
        let x = ((left as f64 - expansion) * scale_x).max(0.0);
        let y = ((top as f64 - expansion) * scale_y).max(0.0);
        let r = ((right as f64 + 1.0 + expansion) * scale_x).min(image_w as f64);
        let b = ((bottom as f64 + 1.0 + expansion) * scale_y).min(image_h as f64);
        boxes.push(OcrBoundingBox {
            x: x as f32,
            y: y as f32,
            width: (r - x) as f32,
            height: (b - y) as f32,
        });
    }
    boxes
}
fn sort_boxes(boxes: &mut [OcrBoundingBox], vertical: bool) {
    if vertical {
        boxes.sort_by(|a, b| b.x.total_cmp(&a.x).then(a.y.total_cmp(&b.y)));
    } else {
        boxes.sort_by(|a, b| a.y.total_cmp(&b.y).then(a.x.total_cmp(&b.x)));
        // Use stable row grouping instead of a non-transitive fuzzy comparator.
        let mut start = 0;
        while start < boxes.len() {
            let baseline = boxes[start].y + boxes[start].height * 0.5;
            let tolerance = boxes[start].height * 0.6;
            let mut end = start + 1;
            while end < boxes.len()
                && ((boxes[end].y + boxes[end].height * 0.5) - baseline).abs() <= tolerance
            {
                end += 1;
            }
            boxes[start..end].sort_by(|a, b| a.x.total_cmp(&b.x));
            start = end;
        }
    }
}
struct Token {
    text: String,
    start: usize,
    end: usize,
}
fn normalize_tokens(tokens: &mut Vec<Token>, options: &PostprocessOptions) {
    let source: String = tokens.iter().map(|token| token.text.as_str()).collect();
    let chinese = match options.punctuation {
        Punctuation::Chinese => true,
        Punctuation::English => false,
        Punctuation::Auto => source.chars().any(is_cjk),
    };
    for token in tokens.iter_mut() {
        token.text = token
            .text
            .chars()
            .map(|ch| match (chinese, ch) {
                (true, ',') => '，',
                (true, ':') => '：',
                (true, ';') => '；',
                (true, '?') => '？',
                (true, '!') => '！',
                (true, '(') => '（',
                (true, ')') => '）',
                (false, '，') => ',',
                (false, '。') => '.',
                (false, '：') => ':',
                (false, '；') => ';',
                (false, '？') => '?',
                (false, '！') => '!',
                (false, '（') => '(',
                (false, '）') => ')',
                _ => ch,
            })
            .collect();
    }
    if chinese {
        for index in 0..tokens.len() {
            if tokens[index].text == "."
                && index > 0
                && tokens[index - 1]
                    .text
                    .chars()
                    .next_back()
                    .is_some_and(is_cjk)
            {
                tokens[index].text = "。".into();
            }
        }
    }
    let mut keep = vec![true; tokens.len()];
    let mut previous_space = true;
    for (index, token) in tokens.iter().enumerate() {
        let space = token.text.chars().all(char::is_whitespace);
        if space {
            let cjk_neighbors = index > 0
                && index + 1 < tokens.len()
                && tokens[index - 1]
                    .text
                    .chars()
                    .next_back()
                    .is_some_and(is_cjk)
                && tokens[index + 1].text.chars().next().is_some_and(is_cjk);
            keep[index] = !options.remove_spaces
                && !previous_space
                && !cjk_neighbors
                && index + 1 < tokens.len();
        }
        previous_space = space;
    }
    let mut index = 0;
    tokens.retain(|_| {
        let retain = keep[index];
        index += 1;
        retain
    });
    while tokens
        .last()
        .is_some_and(|token| token.text.chars().all(char::is_whitespace))
    {
        tokens.pop();
    }
}
fn is_cjk(ch: char) -> bool {
    matches!(ch as u32, 0x3000..=0x30ff | 0x3400..=0x9fff | 0xac00..=0xd7af | 0xf900..=0xfaff | 0x20000..=0x3134f)
}
