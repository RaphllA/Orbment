pub mod action_bus;
pub mod autostart;
pub mod clipboard_content;
pub mod config;
pub mod credentials;
pub mod db;
pub mod diag;
pub mod i18n;
pub mod ipc;
pub mod lifecycle;
pub mod logging;
pub mod ocr;
pub mod single_instance;
pub mod utilities;

pub use ocr::{
    OcrBoundingBox, OcrEngine, OcrLanguage, OcrLine, OcrResult, OcrWord, WindowsNativeOcr,
};
