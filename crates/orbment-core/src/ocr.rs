use crate::i18n::tr;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::Path;
use windows::{
    Globalization::Language,
    Graphics::Imaging::{BitmapAlphaMode, BitmapDecoder, BitmapPixelFormat, SoftwareBitmap},
    Media::Ocr::OcrEngine as NativeOcrEngine,
    Storage::{FileAccessMode, StorageFile},
    Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize},
    core::HSTRING,
};

/// Implementations run synchronously on the caller's background worker. Only pure
/// data crosses the service boundary; native apartment-bound objects stay local.
pub trait OcrEngine: Send + Sync {
    /// Whether the engine's required local assets are available without loading it.
    fn is_ready(&self) -> bool {
        true
    }

    fn recognize(&self, image_path: &Path) -> Result<OcrResult>;
}

/// Decisions are separate from recognition; missing offline assets use Windows
/// OCR without prompting or starting a model download on a worker thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OcrDecision {
    ChooseEngine,
    WindowsNative,
    OfflineOnnx,
    OfflineUnavailable,
}

#[derive(Default)]
pub struct OcrRouter {
    offline: std::sync::RwLock<Option<std::sync::Arc<dyn OcrEngine>>>,
}

impl OcrRouter {
    pub fn register_offline(&self, engine: std::sync::Arc<dyn OcrEngine>) {
        *self
            .offline
            .write()
            .expect("OCR registration lock poisoned") = Some(engine);
    }

    pub fn unregister_offline(&self) {
        *self
            .offline
            .write()
            .expect("OCR registration lock poisoned") = None;
    }

    pub fn offline_ready(&self) -> bool {
        self.offline
            .read()
            .expect("OCR registration lock poisoned")
            .as_ref()
            .is_some_and(|engine| engine.is_ready())
    }

    pub fn decision(&self, configured_tier: &str) -> Result<OcrDecision> {
        Ok(match configured_tier {
            "Ask" | "OfflineOnnx" if !self.offline_ready() => OcrDecision::WindowsNative,
            "Ask" => OcrDecision::ChooseEngine,
            "WindowsNative" => OcrDecision::WindowsNative,
            "OfflineOnnx" => OcrDecision::OfflineOnnx,
            _ => anyhow::bail!("Invalid OCR tier: {configured_tier}"),
        })
    }

    /// The UI persists the user's choice in `app.ocr_engine_tier` before retrying.
    pub fn resolve(&self, configured_tier: &str) -> Result<std::sync::Arc<dyn OcrEngine>> {
        match self.decision(configured_tier)? {
            OcrDecision::WindowsNative => Ok(std::sync::Arc::new(WindowsNativeOcr::default())),
            OcrDecision::OfflineOnnx => self
                .offline
                .read()
                .expect("OCR registration lock poisoned")
                .clone()
                .filter(|engine| engine.is_ready())
                .map_or_else(
                    || Ok(std::sync::Arc::new(WindowsNativeOcr::default()) as _),
                    Ok,
                ),
            OcrDecision::ChooseEngine => anyhow::bail!("{}", tr("ocr.choose_engine")),
            OcrDecision::OfflineUnavailable => anyhow::bail!("{}", tr("ocr.offline_unavailable")),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct OcrBoundingBox {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl OcrBoundingBox {
    fn union(self, other: Self) -> Self {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        Self {
            x,
            y,
            width: (self.x + self.width).max(other.x + other.width) - x,
            height: (self.y + self.height).max(other.y + other.height) - y,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct OcrWord {
    pub text: String,
    pub bounding_box: OcrBoundingBox,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct OcrLine {
    pub text: String,
    pub bounding_box: OcrBoundingBox,
    pub words: Vec<OcrWord>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct OcrResult {
    pub text: String,
    /// Coordinates are physical pixels in the decoded bitmap, with no resizing.
    pub lines: Vec<OcrLine>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct OcrLanguage {
    pub language_tag: String,
    pub display_name: String,
}

/// Construction is inert: Windows OCR is activated only when requested, even
/// when the vision module is disabled. `None` uses the Windows user languages.
#[derive(Clone, Debug, Default)]
pub struct WindowsNativeOcr {
    language: Option<String>,
}

impl WindowsNativeOcr {
    pub fn new(language: Option<String>) -> Self {
        Self { language }
    }

    pub fn available_languages() -> Result<Vec<OcrLanguage>> {
        let _apartment = OcrApartment::initialize()?;
        Self::installed_languages()
    }

    fn installed_languages() -> Result<Vec<OcrLanguage>> {
        NativeOcrEngine::AvailableRecognizerLanguages()
            .with_context(|| tr("ocr.language_detection_failed"))?
            .into_iter()
            .map(|language| {
                Ok(OcrLanguage {
                    language_tag: language.LanguageTag()?.to_string(),
                    display_name: language.DisplayName()?.to_string(),
                })
            })
            .collect()
    }

    fn create_engine(&self) -> Result<NativeOcrEngine> {
        let installed = Self::installed_languages()?;
        let tags = installed
            .iter()
            .map(|language| language.language_tag.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        ensure!(!installed.is_empty(), "{}", tr("ocr.language_pack_missing"));
        if let Some(tag) = &self.language {
            let language = Language::CreateLanguage(&HSTRING::from(tag))
                .with_context(|| format!("{}: {tag}", tr("ocr.invalid_language")))?;
            ensure!(
                NativeOcrEngine::IsLanguageSupported(&language)?,
                "{}: {tag}; [{tags}]",
                tr("ocr.language_pack_missing")
            );
            NativeOcrEngine::TryCreateFromLanguage(&language)
                .with_context(|| format!("{}: {tag}", tr("ocr.engine_creation_failed")))
        } else {
            NativeOcrEngine::TryCreateFromUserProfileLanguages()
                .with_context(|| format!("{}: [{tags}]", tr("ocr.user_language_pack_missing")))
        }
    }

    /// Recognize an already decoded bitmap on a background MTA thread. This does
    /// not take ownership or close the caller's bitmap.
    pub fn recognize_bitmap(&self, bitmap: &SoftwareBitmap) -> Result<OcrResult> {
        let _apartment = OcrApartment::initialize()?;
        let engine = self.create_engine()?;
        Self::check_dimensions(bitmap.PixelWidth()? as u32, bitmap.PixelHeight()? as u32)?;
        let converted = SoftwareBitmap::ConvertWithAlpha(
            bitmap,
            BitmapPixelFormat::Bgra8,
            BitmapAlphaMode::Ignore,
        )
        .with_context(|| tr("ocr.decode_failed"))?;
        Self::recognize_native(&engine, &converted)
    }

    fn check_dimensions(width: u32, height: u32) -> Result<()> {
        let maximum = NativeOcrEngine::MaxImageDimension()?;
        ensure!(
            width > 0 && height > 0 && width <= maximum && height <= maximum,
            "{}: {width}x{height}; {maximum}",
            tr("ocr.image_dimensions_unsupported")
        );
        Ok(())
    }

    fn recognize_native(engine: &NativeOcrEngine, bitmap: &SoftwareBitmap) -> Result<OcrResult> {
        let result = engine
            .RecognizeAsync(bitmap)?
            .get()
            .with_context(|| tr("ocr.recognition_failed"))?;
        let mut lines = Vec::new();
        for line in result.Lines()? {
            let mut words = Vec::new();
            for word in line.Words()? {
                let rect = word.BoundingRect()?;
                words.push(OcrWord {
                    text: word.Text()?.to_string(),
                    bounding_box: OcrBoundingBox {
                        x: rect.X,
                        y: rect.Y,
                        width: rect.Width,
                        height: rect.Height,
                    },
                });
            }
            // Windows supplies word rectangles, not a line rectangle. The union
            // retains the original image coordinates, including interword gaps.
            let bounding_box = words
                .iter()
                .map(|word| word.bounding_box)
                .reduce(OcrBoundingBox::union)
                .unwrap_or_default();
            lines.push(OcrLine {
                text: line.Text()?.to_string(),
                bounding_box,
                words,
            });
        }
        Ok(OcrResult {
            text: result.Text()?.to_string(),
            lines,
        })
    }
}

impl OcrEngine for WindowsNativeOcr {
    fn recognize(&self, image_path: &Path) -> Result<OcrResult> {
        let _apartment = OcrApartment::initialize()?;
        let engine = self.create_engine()?;
        let path = std::path::absolute(image_path)
            .with_context(|| format!("{}: {}", tr("ocr.open_failed"), image_path.display()))?;
        let file = StorageFile::GetFileFromPathAsync(&HSTRING::from(path.as_os_str()))?
            .get()
            .with_context(|| format!("{}: {}", tr("ocr.open_failed"), path.display()))?;
        let stream = file.OpenAsync(FileAccessMode::Read)?.get()?;
        let decoder = BitmapDecoder::CreateAsync(&stream)?
            .get()
            .with_context(|| tr("ocr.decode_failed"))?;
        // Reject oversized decoded images before allocating their pixel buffer.
        Self::check_dimensions(decoder.PixelWidth()?, decoder.PixelHeight()?)?;
        let bitmap = decoder
            .GetSoftwareBitmapConvertedAsync(BitmapPixelFormat::Bgra8, BitmapAlphaMode::Ignore)?
            .get()
            .with_context(|| tr("ocr.decode_failed"))?;
        Self::recognize_native(&engine, &bitmap)
    }
}

struct OcrApartment;

impl OcrApartment {
    fn initialize() -> Result<Self> {
        unsafe { RoInitialize(RO_INIT_MULTITHREADED) }
            .with_context(|| tr("ocr.background_thread_required"))?;
        Ok(Self)
    }
}

impl Drop for OcrApartment {
    fn drop(&mut self) {
        unsafe { RoUninitialize() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

    struct OfflineEngine(Arc<AtomicBool>);

    impl OcrEngine for OfflineEngine {
        fn is_ready(&self) -> bool {
            self.0.load(Ordering::Relaxed)
        }

        fn recognize(&self, _: &Path) -> Result<OcrResult> {
            Ok(OcrResult::default())
        }
    }

    #[test]
    fn missing_offline_assets_fall_back_without_prompting() {
        let router = OcrRouter::default();
        for tier in ["Ask", "OfflineOnnx"] {
            assert_eq!(router.decision(tier).unwrap(), OcrDecision::WindowsNative);
            assert!(router.resolve(tier).is_ok());
        }
        let ready = Arc::new(AtomicBool::new(false));
        let engine: Arc<dyn OcrEngine> = Arc::new(OfflineEngine(ready.clone()));
        router.register_offline(engine.clone());
        for tier in ["Ask", "OfflineOnnx"] {
            assert_eq!(router.decision(tier).unwrap(), OcrDecision::WindowsNative);
            assert!(router.resolve(tier).is_ok());
        }
        ready.store(true, Ordering::Relaxed);
        assert_eq!(router.decision("Ask").unwrap(), OcrDecision::ChooseEngine);
        assert!(router.resolve("Ask").is_err());
        assert_eq!(router.decision("OfflineOnnx").unwrap(), OcrDecision::OfflineOnnx);
        assert!(Arc::ptr_eq(&router.resolve("OfflineOnnx").unwrap(), &engine));
        router.unregister_offline();
        assert_eq!(router.decision("OfflineOnnx").unwrap(), OcrDecision::WindowsNative);
        assert!(router.decision("invalid").is_err());
    }
}
