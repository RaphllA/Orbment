use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub fn contains(self, point: Point) -> bool {
        point.x >= self.x
            && point.y >= self.y
            && point.x < self.x + self.width
            && point.y < self.y + self.height
    }
    pub fn intersection(self, other: Self) -> Option<Self> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let width = (self.x + self.width).min(other.x + other.width) - x;
        let height = (self.y + self.height).min(other.y + other.height) - y;
        (width > 0.0 && height > 0.0).then_some(Self {
            x,
            y,
            width,
            height,
        })
    }
    pub fn from_points(a: Point, b: Point) -> Self {
        Self {
            x: a.x.min(b.x),
            y: a.y.min(b.y),
            width: (a.x - b.x).abs(),
            height: (a.y - b.y).abs(),
        }
    }
    pub fn validate(self) -> Result<Self> {
        ensure!(
            [self.x, self.y, self.width, self.height]
                .iter()
                .all(|v| v.is_finite())
                && self.width >= 1.0
                && self.height >= 1.0
                && self.width <= 32768.0
                && self.height <= 32768.0,
            "Invalid physical capture rectangle"
        );
        ensure!(
            self.x >= i32::MIN as f64
                && self.y >= i32::MIN as f64
                && self.x + self.width <= i32::MAX as f64
                && self.y + self.height <= i32::MAX as f64,
            "Capture coordinates exceed Win32 range"
        );
        Ok(self)
    }
}

/// Tightly packed, top-down BGRA8; alpha is straight except at the layered-window boundary.
#[derive(Clone, Debug)]
pub struct CapturedImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl CapturedImage {
    pub fn validate(&self) -> Result<()> {
        let expected = (self.width as usize)
            .checked_mul(self.height as usize)
            .and_then(|v| v.checked_mul(4));
        ensure!(
            self.width > 0 && self.height > 0 && expected == Some(self.pixels.len()),
            "Invalid BGRA image dimensions or byte length"
        );
        Ok(())
    }
    pub fn crop(&self, rect: Rect) -> Result<Self> {
        rect.validate()?;
        ensure!(
            rect.x >= 0.0
                && rect.y >= 0.0
                && rect.x + rect.width <= self.width as f64
                && rect.y + rect.height <= self.height as f64,
            "Crop is outside captured image"
        );
        let (x, y, width, height) = (
            rect.x as usize,
            rect.y as usize,
            rect.width as u32,
            rect.height as u32,
        );
        let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
        for row in y..y + height as usize {
            let start = (row * self.width as usize + x) * 4;
            pixels.extend_from_slice(&self.pixels[start..start + width as usize * 4]);
        }
        Ok(Self {
            width,
            height,
            pixels,
        })
    }
    pub fn rgba(&self) -> Vec<u8> {
        let mut pixels = self.pixels.clone();
        for pixel in pixels.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
        }
        pixels
    }
    pub fn from_rgba(width: u32, height: u32, mut pixels: Vec<u8>) -> Result<Self> {
        for pixel in pixels.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
        }
        let image = Self {
            width,
            height,
            pixels,
        };
        image.validate()?;
        Ok(image)
    }
}

impl CapturedImage {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let image = image::open(path)?.into_rgba8();
        Self::from_rgba(image.width(), image.height(), image.into_raw())
    }
    pub fn save_png(&self, path: &std::path::Path) -> Result<()> {
        self.validate()?;
        image::save_buffer_with_format(
            path,
            &self.rgba(),
            self.width,
            self.height,
            image::ColorType::Rgba8,
            image::ImageFormat::Png,
        )?;
        Ok(())
    }
    /// Save without replacing an existing file, adding a numeric suffix on collision.
    pub fn save_png_unique(&self, path: &std::path::Path) -> Result<std::path::PathBuf> {
        use std::io::Write;

        let bytes = self.png_bytes()?;
        let stem = path.file_stem().ok_or_else(|| {
            anyhow::anyhow!("Image destination has no filename: {}", path.display())
        })?;
        let mut candidate = path.to_owned();
        let mut suffix = 0u64;
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(mut file) => {
                    let result = file.write_all(&bytes);
                    drop(file);
                    if let Err(error) = result {
                        let _ = std::fs::remove_file(&candidate);
                        return Err(error.into());
                    }
                    return Ok(candidate);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    suffix = suffix
                        .checked_add(1)
                        .ok_or_else(|| anyhow::anyhow!("Image filename suffix exhausted"))?;
                    let mut filename = stem.to_os_string();
                    filename.push(format!("_{suffix}"));
                    if let Some(extension) = path.extension() {
                        filename.push(".");
                        filename.push(extension);
                    }
                    candidate = path.with_file_name(filename);
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
    pub fn png_bytes(&self) -> Result<Vec<u8>> {
        use image::ImageEncoder;
        self.validate()?;
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes).write_image(
            &self.rgba(),
            self.width,
            self.height,
            image::ExtendedColorType::Rgba8,
        )?;
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::CapturedImage;

    #[test]
    fn unique_png_saves_preserve_existing_files_and_concurrent_exports() {
        let directory = std::env::temp_dir().join(format!(
            "orbment-pin-save-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let image = CapturedImage::from_rgba(1, 1, vec![12, 34, 56, 255]).unwrap();
        let path = directory.join("capture.png");
        assert_eq!(image.save_png_unique(&path).unwrap(), path);
        let original = std::fs::read(&path).unwrap();
        let occupied = directory.join("capture_1.png");
        std::fs::write(&occupied, b"existing export").unwrap();

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let image = image.clone();
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    image.save_png_unique(&path).unwrap()
                })
            })
            .collect();
        let mut saved: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        saved.sort();
        saved.dedup();
        assert_eq!(saved.len(), 4);
        for (index, saved_path) in saved.iter().enumerate() {
            assert_eq!(
                saved_path,
                &directory.join(format!("capture_{}.png", index + 2))
            );
            assert_eq!(std::fs::read(saved_path).unwrap(), original);
        }
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(std::fs::read(&occupied).unwrap(), b"existing export");
        assert!(
            image
                .save_png_unique(&directory.join("missing/file.png"))
                .is_err()
        );
        let invalid = CapturedImage {
            width: 0,
            height: 0,
            pixels: Vec::new(),
        };
        let invalid_path = directory.join("invalid.png");
        assert!(invalid.save_png_unique(&invalid_path).is_err());
        assert!(!invalid_path.exists());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
