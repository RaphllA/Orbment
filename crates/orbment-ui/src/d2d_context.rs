use anyhow::{Context, Result};
use std::{marker::PhantomData, rc::Rc};
use windows::{
    Win32::{
        Foundation::{HMODULE, RECT},
        Graphics::{
            Direct2D::{Common::*, *},
            Direct3D::*,
            Direct3D11::*,
            DirectWrite::*,
            Dxgi::{Common::*, IDXGIDevice},
            Gdi::HDC,
        },
    },
    core::{Interface, PCWSTR, w},
};

pub struct D2dContext {
    _d3d: ID3D11Device,
    _immediate: ID3D11DeviceContext,
    _device: ID2D1Device,
    factory: ID2D1Factory1,
    write: IDWriteFactory,
    _main_thread: PhantomData<Rc<()>>,
}

impl D2dContext {
    pub fn d3d_device(&self) -> &ID3D11Device {
        &self._d3d
    }
    pub fn immediate_context(&self) -> &ID3D11DeviceContext {
        &self._immediate
    }
    pub fn d2d_device(&self) -> &ID2D1Device {
        &self._device
    }
    pub fn write_factory(&self) -> &IDWriteFactory {
        &self.write
    }

    pub fn new() -> Result<Self> {
        unsafe {
            let mut device = None;
            let mut immediate = None;
            let create = |driver, device: &mut _, immediate: &mut _| {
                D3D11CreateDevice(
                    None,
                    driver,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    None,
                    D3D11_SDK_VERSION,
                    Some(device),
                    None,
                    Some(immediate),
                )
            };
            if let Err(error) = create(D3D_DRIVER_TYPE_HARDWARE, &mut device, &mut immediate) {
                tracing::warn!(%error, "Hardware graphics unavailable; using WARP");
                create(D3D_DRIVER_TYPE_WARP, &mut device, &mut immediate)?;
            }
            let d3d = device.context("D3D11 returned no device")?;
            let immediate = immediate.context("D3D11 returned no immediate context")?;
            let factory: ID2D1Factory1 =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dxgi: IDXGIDevice = d3d.cast()?;
            let device = factory.CreateDevice(&dxgi)?;
            let write = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            Ok(Self {
                _d3d: d3d,
                _immediate: immediate,
                _device: device,
                factory,
                write,
                _main_thread: PhantomData,
            })
        }
    }

    pub fn text_format(&self, font: &str, size: f32, style: u32) -> Result<IDWriteTextFormat> {
        let font = crate::wide(font);
        unsafe {
            let format = self.write.CreateTextFormat(
                PCWSTR(font.as_ptr()),
                None,
                if style & 1 != 0 {
                    DWRITE_FONT_WEIGHT_BOLD
                } else {
                    DWRITE_FONT_WEIGHT_NORMAL
                },
                if style & 2 != 0 {
                    DWRITE_FONT_STYLE_ITALIC
                } else {
                    DWRITE_FONT_STYLE_NORMAL
                },
                DWRITE_FONT_STRETCH_NORMAL,
                size * 96.0 / 72.0,
                w!("zh-CN"),
            )?;
            format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            Ok(format)
        }
    }

    pub fn measure(&self, text: &str, format: &IDWriteTextFormat) -> Result<(f32, f32)> {
        let text: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            let layout = self.write.CreateTextLayout(&text, format, 4096.0, 1024.0)?;
            let mut metrics = DWRITE_TEXT_METRICS::default();
            layout.GetMetrics(&mut metrics)?;
            Ok((metrics.widthIncludingTrailingWhitespace, metrics.height))
        }
    }

    pub fn target(&self) -> Result<ID2D1DCRenderTarget> {
        unsafe {
            Ok(self
                .factory
                .CreateDCRenderTarget(&D2D1_RENDER_TARGET_PROPERTIES {
                    // DC targets retain only this window's bitmap; no per-window swap chain.
                    r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                    pixelFormat: D2D1_PIXEL_FORMAT {
                        format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                    },
                    dpiX: 96.0,
                    dpiY: 96.0,
                    usage: D2D1_RENDER_TARGET_USAGE_NONE,
                    minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
                })?)
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TextRun {
    pub text: String,
    pub color: u32,
    /// Optional background fill fraction, from zero to one.
    pub progress: Option<f32>,
    pub progress_color: u32,
}

pub(crate) struct TextStyle<'a> {
    pub format: &'a IDWriteTextFormat,
    pub decorations: u32,
    pub foreground: u32,
    pub background: u32,
    pub runs: &'a [TextRun],
}

pub(crate) fn draw(
    context: &D2dContext,
    target: &ID2D1DCRenderTarget,
    dc: HDC,
    bounds: RECT,
    dpi: u32,
    text: &str,
    style: &TextStyle<'_>,
) -> windows::core::Result<()> {
    unsafe {
        target.BindDC(dc, &bounds)?;
        target.SetDpi(dpi as f32, dpi as f32);
        target.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
        let brush = target.CreateSolidColorBrush(&color(style.foreground), None)?;
        let scale = 96.0 / dpi as f32;
        let text: Vec<u16> = text.encode_utf16().collect();
        let layout = context.write.CreateTextLayout(
            &text,
            style.format,
            (bounds.right - bounds.left) as f32 * scale,
            (bounds.bottom - bounds.top) as f32 * scale,
        )?;
        let range = DWRITE_TEXT_RANGE {
            startPosition: 0,
            length: text.len() as u32,
        };
        layout.SetUnderline(style.decorations & 4 != 0, range)?;
        layout.SetStrikethrough(style.decorations & 8 != 0, range)?;
        let mut position = 0;
        let mut progress_rectangles = Vec::new();
        for run in style.runs {
            let length = run.text.encode_utf16().count() as u32;
            let range = DWRITE_TEXT_RANGE {
                startPosition: position,
                length,
            };
            position += length;
            if length == 0 {
                continue;
            }
            let foreground = target.CreateSolidColorBrush(&color(run.color), None)?;
            layout.SetDrawingEffect(&foreground, range)?;
            if let Some(progress) = run.progress.filter(|value| *value > 0.0) {
                let background = target.CreateSolidColorBrush(&color(run.progress_color), None)?;
                let mut hits = vec![DWRITE_HIT_TEST_METRICS::default(); length as usize];
                let mut count = 0;
                layout.HitTestTextRange(
                    range.startPosition,
                    range.length,
                    0.0,
                    0.0,
                    Some(&mut hits),
                    &mut count,
                )?;
                for hit in hits.into_iter().take(count as usize) {
                    progress_rectangles.push((
                        D2D_RECT_F {
                            left: hit.left,
                            top: hit.top,
                            right: hit.left + hit.width * progress.min(1.0),
                            bottom: hit.top + hit.height,
                        },
                        background.clone(),
                    ));
                }
            }
        }
        target.BeginDraw();
        target.Clear(Some(&color(style.background)));
        for (rect, brush) in progress_rectangles {
            target.FillRectangle(&rect, &brush);
        }
        target.DrawTextLayout(
            Default::default(),
            &layout,
            &brush,
            D2D1_DRAW_TEXT_OPTIONS_CLIP,
        );
        target.EndDraw(None, None)
    }
}

fn color(value: u32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: (value & 255) as f32 / 255.0,
        g: ((value >> 8) & 255) as f32 / 255.0,
        b: ((value >> 16) & 255) as f32 / 255.0,
        a: 1.0,
    }
}
