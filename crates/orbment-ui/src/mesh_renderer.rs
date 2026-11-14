use anyhow::{Context, Result, ensure};
use egui::{
    ClippedPrimitive, TextureId,
    epaint::{ImageData, ImageDelta, Primitive},
};
use std::{collections::HashMap, mem::size_of};
use windows::{
    Win32::{
        Foundation::RECT,
        Graphics::{Direct3D::Fxc::*, Direct3D::*, Direct3D11::*, Dxgi::Common::*},
    },
    core::s,
};

const SHADER: &str = r#"
cbuffer Screen : register(b0) { float2 screen; float2 padding; };
struct In { float2 pos : POSITION; float2 uv : TEXCOORD0; float4 color : COLOR0; };
struct Out { float4 pos : SV_POSITION; float2 uv : TEXCOORD0; float4 color : COLOR0; };
Out vs_main(In v) { Out o; o.pos=float4(2*v.pos.x/screen.x-1,1-2*v.pos.y/screen.y,0,1); o.uv=v.uv; o.color=v.color; return o; }
Texture2D tex : register(t0); SamplerState smp : register(s0);
float4 ps_main(Out v) : SV_TARGET { return v.color * tex.Sample(smp,v.uv); }
"#;

#[repr(C)]
struct Vertex {
    position: [f32; 2],
    uv: [f32; 2],
    color: [u8; 4],
}

struct Texture {
    size: [usize; 2],
    pixels: Vec<[u8; 4]>,
    options: egui::TextureOptions,
    view: Option<ID3D11ShaderResourceView>,
    sampler: Option<ID3D11SamplerState>,
    dirty: bool,
}

/// GPU objects are disposable; the active texture atlas remains recoverable on CPU.
pub(crate) struct MeshRenderer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vertex: ID3D11VertexShader,
    pixel: ID3D11PixelShader,
    layout: ID3D11InputLayout,
    blend: ID3D11BlendState,
    rasterizer: ID3D11RasterizerState,
    constants: ID3D11Buffer,
    textures: HashMap<TextureId, Texture>,
}

impl MeshRenderer {
    pub fn new(device: &ID3D11Device, context: &ID3D11DeviceContext) -> Result<Self> {
        unsafe {
            let vs = compile(s!("vs_main"), s!("vs_4_0"))?;
            let ps = compile(s!("ps_main"), s!("ps_4_0"))?;
            let mut vertex = None;
            let mut pixel = None;
            device.CreateVertexShader(&vs, None, Some(&mut vertex))?;
            device.CreatePixelShader(&ps, None, Some(&mut pixel))?;
            let elements = [
                D3D11_INPUT_ELEMENT_DESC {
                    SemanticName: s!("POSITION"),
                    Format: DXGI_FORMAT_R32G32_FLOAT,
                    AlignedByteOffset: 0,
                    ..Default::default()
                },
                D3D11_INPUT_ELEMENT_DESC {
                    SemanticName: s!("TEXCOORD"),
                    Format: DXGI_FORMAT_R32G32_FLOAT,
                    AlignedByteOffset: 8,
                    ..Default::default()
                },
                D3D11_INPUT_ELEMENT_DESC {
                    SemanticName: s!("COLOR"),
                    Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                    AlignedByteOffset: 16,
                    ..Default::default()
                },
            ];
            let mut layout = None;
            device.CreateInputLayout(&elements, &vs, Some(&mut layout))?;
            let mut desc = D3D11_BLEND_DESC::default();
            desc.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
                BlendEnable: true.into(),
                SrcBlend: D3D11_BLEND_ONE,
                DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOp: D3D11_BLEND_OP_ADD,
                SrcBlendAlpha: D3D11_BLEND_ONE,
                DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOpAlpha: D3D11_BLEND_OP_ADD,
                RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
            };
            let mut blend = None;
            device.CreateBlendState(&desc, Some(&mut blend))?;
            let mut rasterizer = None;
            device.CreateRasterizerState(
                &D3D11_RASTERIZER_DESC {
                    FillMode: D3D11_FILL_SOLID,
                    CullMode: D3D11_CULL_NONE,
                    ScissorEnable: true.into(),
                    DepthClipEnable: true.into(),
                    ..Default::default()
                },
                Some(&mut rasterizer),
            )?;
            let constants = buffer(device, &[0.0_f32; 4], D3D11_BIND_CONSTANT_BUFFER)?;
            Ok(Self {
                device: device.clone(),
                context: context.clone(),
                vertex: vertex.context("CreateVertexShader returned null")?,
                pixel: pixel.context("CreatePixelShader returned null")?,
                layout: layout.context("CreateInputLayout returned null")?,
                blend: blend.context("CreateBlendState returned null")?,
                rasterizer: rasterizer.context("CreateRasterizerState returned null")?,
                constants,
                textures: HashMap::new(),
            })
        }
    }

    pub fn rebuild(&mut self, device: &ID3D11Device, context: &ID3D11DeviceContext) -> Result<()> {
        let mut next = Self::new(device, context)?;
        for texture in self.textures.values_mut() {
            texture.view = None;
            texture.sampler = None;
            texture.dirty = true;
        }
        next.textures = std::mem::take(&mut self.textures);
        *self = next;
        self.upload_pending()?;
        Ok(())
    }

    pub fn update(&mut self, id: TextureId, delta: &ImageDelta) -> Result<()> {
        let size = delta.image.size();
        let pixels: Vec<[u8; 4]> = match &delta.image {
            ImageData::Color(image) => image.pixels.iter().map(|pixel| pixel.to_array()).collect(),
            ImageData::Font(image) => image
                .srgba_pixels(None)
                .map(|pixel| pixel.to_array())
                .collect(),
        };
        if let Some([x, y]) = delta.pos {
            let texture = self
                .textures
                .get_mut(&id)
                .context("Texture patch has no base image")?;
            ensure!(
                x + size[0] <= texture.size[0] && y + size[1] <= texture.size[1],
                "Texture patch exceeds image"
            );
            for row in 0..size[1] {
                let start = (y + row) * texture.size[0] + x;
                texture.pixels[start..start + size[0]]
                    .copy_from_slice(&pixels[row * size[0]..(row + 1) * size[0]]);
            }
            texture.options = delta.options;
            texture.dirty = true;
        } else {
            self.textures.insert(
                id,
                Texture {
                    size,
                    pixels,
                    options: delta.options,
                    view: None,
                    sampler: None,
                    dirty: true,
                },
            );
        }
        Ok(())
    }

    pub fn upload_pending(&mut self) -> Result<()> {
        for texture in self.textures.values_mut().filter(|texture| texture.dirty) {
            upload(&self.device, texture)?;
            texture.dirty = false;
        }
        Ok(())
    }

    pub fn free(&mut self, ids: &[TextureId]) {
        for id in ids {
            self.textures.remove(id);
        }
    }

    pub fn paint(
        &self,
        target: &ID3D11RenderTargetView,
        size: [u32; 2],
        scale: f32,
        meshes: &[ClippedPrimitive],
    ) -> Result<()> {
        unsafe {
            let ctx = &self.context;
            let constants = [size[0] as f32 / scale, size[1] as f32 / scale, 0.0, 0.0];
            ctx.UpdateSubresource(&self.constants, 0, None, constants.as_ptr().cast(), 0, 0);
            ctx.OMSetRenderTargets(Some(&[Some(target.clone())]), None);
            ctx.ClearRenderTargetView(target, &[0.08, 0.08, 0.08, 1.0]);
            ctx.OMSetBlendState(&self.blend, None, u32::MAX);
            ctx.OMSetDepthStencilState(None, 0);
            ctx.RSSetState(&self.rasterizer);
            ctx.RSSetViewports(Some(&[D3D11_VIEWPORT {
                Width: size[0] as f32,
                Height: size[1] as f32,
                MaxDepth: 1.0,
                ..Default::default()
            }]));
            ctx.IASetInputLayout(&self.layout);
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.VSSetShader(&self.vertex, None);
            ctx.VSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            ctx.PSSetShader(&self.pixel, None);
            ctx.GSSetShader(None, None);
            for primitive in meshes {
                let Primitive::Mesh(mesh) = &primitive.primitive else {
                    anyhow::bail!("Custom egui callbacks require the Direct2D canvas API");
                };
                if mesh.indices.is_empty() {
                    continue;
                }
                let texture = self
                    .textures
                    .get(&mesh.texture_id)
                    .context("Mesh references an unavailable texture")?;
                let vertices: Vec<Vertex> = mesh
                    .vertices
                    .iter()
                    .map(|v| Vertex {
                        position: [v.pos.x, v.pos.y],
                        uv: [v.uv.x, v.uv.y],
                        color: v.color.to_array(),
                    })
                    .collect();
                let vertex = Some(buffer(&self.device, &vertices, D3D11_BIND_VERTEX_BUFFER)?);
                let index = buffer(&self.device, &mesh.indices, D3D11_BIND_INDEX_BUFFER)?;
                ctx.IASetVertexBuffers(
                    0,
                    1,
                    Some(&vertex),
                    Some(&(size_of::<Vertex>() as u32)),
                    Some(&0),
                );
                ctx.IASetIndexBuffer(&index, DXGI_FORMAT_R32_UINT, 0);
                ctx.PSSetShaderResources(0, Some(std::slice::from_ref(&texture.view)));
                ctx.PSSetSamplers(0, Some(std::slice::from_ref(&texture.sampler)));
                let rect = primitive.clip_rect;
                let clip = RECT {
                    left: (rect.min.x * scale).floor().clamp(0.0, size[0] as f32) as i32,
                    top: (rect.min.y * scale).floor().clamp(0.0, size[1] as f32) as i32,
                    right: (rect.max.x * scale).ceil().clamp(0.0, size[0] as f32) as i32,
                    bottom: (rect.max.y * scale).ceil().clamp(0.0, size[1] as f32) as i32,
                };
                if clip.right <= clip.left || clip.bottom <= clip.top {
                    continue;
                }
                ctx.RSSetScissorRects(Some(&[clip]));
                ctx.DrawIndexed(mesh.indices.len() as u32, 0, 0);
            }
            // Release immediate-context references so closing a host releases all its resources.
            ctx.ClearState();
        }
        Ok(())
    }
}

fn buffer<T>(device: &ID3D11Device, data: &[T], binding: D3D11_BIND_FLAG) -> Result<ID3D11Buffer> {
    let bytes = std::mem::size_of_val(data);
    ensure!(
        bytes > 0 && bytes <= u32::MAX as usize,
        "Invalid D3D11 buffer length"
    );
    let mut result = None;
    unsafe {
        device.CreateBuffer(
            &D3D11_BUFFER_DESC {
                ByteWidth: bytes as u32,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: binding.0 as u32,
                ..Default::default()
            },
            Some(&D3D11_SUBRESOURCE_DATA {
                pSysMem: data.as_ptr().cast(),
                ..Default::default()
            }),
            Some(&mut result),
        )?;
    }
    result.context("CreateBuffer returned null")
}

fn upload(device: &ID3D11Device, texture: &mut Texture) -> Result<()> {
    unsafe {
        let mut image = None;
        device.CreateTexture2D(
            &D3D11_TEXTURE2D_DESC {
                Width: texture.size[0] as u32,
                Height: texture.size[1] as u32,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_IMMUTABLE,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                ..Default::default()
            },
            Some(&D3D11_SUBRESOURCE_DATA {
                pSysMem: texture.pixels.as_ptr().cast(),
                SysMemPitch: texture.size[0] as u32 * 4,
                ..Default::default()
            }),
            Some(&mut image),
        )?;
        let image = image.context("CreateTexture2D returned null")?;
        device.CreateShaderResourceView(&image, None, Some(&mut texture.view))?;
        let address = match texture.options.wrap_mode {
            egui::TextureWrapMode::ClampToEdge => D3D11_TEXTURE_ADDRESS_CLAMP,
            egui::TextureWrapMode::Repeat => D3D11_TEXTURE_ADDRESS_WRAP,
            egui::TextureWrapMode::MirroredRepeat => D3D11_TEXTURE_ADDRESS_MIRROR,
        };
        device.CreateSamplerState(
            &D3D11_SAMPLER_DESC {
                Filter: match (texture.options.minification, texture.options.magnification) {
                    (egui::TextureFilter::Nearest, egui::TextureFilter::Nearest) => {
                        D3D11_FILTER_MIN_MAG_MIP_POINT
                    }
                    (egui::TextureFilter::Nearest, egui::TextureFilter::Linear) => {
                        D3D11_FILTER_MIN_POINT_MAG_LINEAR_MIP_POINT
                    }
                    (egui::TextureFilter::Linear, egui::TextureFilter::Nearest) => {
                        D3D11_FILTER_MIN_LINEAR_MAG_MIP_POINT
                    }
                    _ => D3D11_FILTER_MIN_MAG_LINEAR_MIP_POINT,
                },
                AddressU: address,
                AddressV: address,
                AddressW: address,
                ComparisonFunc: D3D11_COMPARISON_ALWAYS,
                MaxLOD: f32::MAX,
                ..Default::default()
            },
            Some(&mut texture.sampler),
        )?;
    }
    Ok(())
}

fn compile(entry: windows::core::PCSTR, target: windows::core::PCSTR) -> Result<Vec<u8>> {
    unsafe {
        let mut code = None;
        let mut errors = None;
        let result = D3DCompile(
            SHADER.as_ptr().cast(),
            SHADER.len(),
            s!("orbment-egui"),
            None,
            None,
            entry,
            target,
            D3DCOMPILE_ENABLE_STRICTNESS,
            0,
            &mut code,
            Some(&mut errors),
        );
        if let Err(error) = result {
            let detail = errors
                .map(|blob: ID3DBlob| {
                    String::from_utf8_lossy(std::slice::from_raw_parts(
                        blob.GetBufferPointer().cast(),
                        blob.GetBufferSize(),
                    ))
                    .into_owned()
                })
                .unwrap_or_default();
            anyhow::bail!("D3D11 shader compilation: {error}: {detail}");
        }
        let code = code.context("D3DCompile returned no shader")?;
        Ok(
            std::slice::from_raw_parts(code.GetBufferPointer().cast(), code.GetBufferSize())
                .to_vec(),
        )
    }
}
