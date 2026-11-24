//! GPU stage: BGRA capture texture -> NV12 encoder input texture.
//!
//! Two full-screen draws with a runtime-compiled pixel shader. The capture
//! texture stays on the GPU; the encoder reads the NV12 texture directly, so
//! nothing is copied to system memory.

use std::collections::VecDeque;

use windows::core::{Interface, PCSTR};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

const VS_SRC: &str = r#"
struct VSOut { float4 pos : SV_POSITION; float2 uv : TEXCOORD0; };
VSOut main(uint id : SV_VertexID) {
    float2 uv = float2((id == 1) ? 2.0 : 0.0, (id == 2) ? 2.0 : 0.0);
    VSOut o;
    o.pos = float4(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    o.uv = uv;
    return o;
}
"#;

const PS_Y_SRC: &str = r#"
Texture2D src : register(t0);
SamplerState smp : register(s0);
float main(float4 pos : SV_POSITION, float2 uv : TEXCOORD0) : SV_TARGET {
    float3 c = src.Sample(smp, uv).rgb;
    float y = dot(c, float3(0.2126, 0.7152, 0.0722));
    return y * (219.0 / 255.0) + (16.0 / 255.0);
}
"#;

const PS_UV_SRC: &str = r#"
Texture2D src : register(t0);
SamplerState smp : register(s0);
// BT.709 chroma: U = (B - Y) / 1.8556, V = (R - Y) / 1.5748.
// The dot products below are those ratios expanded with BT.709 luma weights.
float2 main(float4 pos : SV_POSITION, float2 uv : TEXCOORD0) : SV_TARGET {
    float3 c = src.Sample(smp, uv).rgb;
    float u = dot(c, float3(-0.11457, -0.38534, 0.50000)) + 0.5;
    float v = dot(c, float3( 0.50000, -0.45425, -0.04585)) + 0.5;
    return float2(u, v) * (224.0 / 255.0) + (16.0 / 255.0);
}
"#;

struct Target {
    texture: ID3D11Texture2D,
    y_rtv: ID3D11RenderTargetView,
    uv_rtv: ID3D11RenderTargetView,
}

pub struct Nv12Converter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps_y: ID3D11PixelShader,
    ps_uv: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    rasterizer: ID3D11RasterizerState,
    targets: Vec<Target>,
    /// Only textures whose encoded packet has come back may be reused.
    free_slots: VecDeque<usize>,
    pub src_w: u32,
    pub src_h: u32,
    pub dst_w: u32,
    pub dst_h: u32,
}

impl Nv12Converter {
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        src_w: u32,
        src_h: u32,
        dst_w: u32,
        dst_h: u32,
    ) -> Result<Self, String> {
        let vs = compile_shader(device, VS_SRC, "main", "vs_5_0")?;
        let ps_y = compile_shader(device, PS_Y_SRC, "main", "ps_5_0")?;
        let ps_uv = compile_shader(device, PS_UV_SRC, "main", "ps_5_0")?;

        let mut sampler: Option<ID3D11SamplerState> = None;
        let desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            ComparisonFunc: D3D11_COMPARISON_NEVER,
            MinLOD: 0.0,
            MaxLOD: f32::MAX,
            ..Default::default()
        };
        unsafe { device.CreateSamplerState(&desc, Some(&mut sampler)) }
            .map_err(|e| format!("CreateSamplerState: {e}"))?;

        let r_desc = D3D11_RASTERIZER_DESC {
            FillMode: D3D11_FILL_SOLID,
            CullMode: D3D11_CULL_NONE,
            FrontCounterClockwise: false.into(),
            DepthClipEnable: false.into(),
            ..Default::default()
        };
        let mut rasterizer: Option<ID3D11RasterizerState> = None;
        unsafe { device.CreateRasterizerState(&r_desc, Some(&mut rasterizer)) }
            .map_err(|e| format!("CreateRasterizerState: {e}"))?;
        let rasterizer = rasterizer.ok_or("rasterizer null")?;

        let mut targets = Vec::with_capacity(3);
        for _ in 0..3 {
            targets.push(create_target(device, dst_w, dst_h)?);
        }

        Ok(Self {
            device: device.clone(),
            context: context.clone(),
            vs,
            ps_y,
            ps_uv,
            sampler: sampler.ok_or("sampler creation returned null")?,
            rasterizer,
            targets,
            free_slots: (0..3).collect(),
            src_w,
            src_h,
            dst_w,
            dst_h,
        })
    }

    /// Convert and scale one capture texture. Returns the NV12 texture to feed
    /// the encoder, plus its pool slot so the caller knows when it is reusable.
    pub fn convert(
        &mut self,
        source: &ID3D11Texture2D,
    ) -> Result<(ID3D11Texture2D, usize), String> {
        let mut srv: Option<ID3D11ShaderResourceView> = None;
        unsafe {
            self.device
                .CreateShaderResourceView(source, None, Some(&mut srv))
        }
        .map_err(|e| format!("CreateShaderResourceView: {e}"))?;
        let srv = srv.ok_or("source SRV null")?;

        let slot = self
            .free_slots
            .pop_front()
            .ok_or("all NV12 textures are still in use")?;
        let t = &self.targets[slot];

        unsafe {
            let ctx = &self.context;
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.RSSetState(Some(&self.rasterizer));
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShaderResources(0, Some(&[Some(srv.clone())]));
            ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));

            // Luma, full resolution.
            let vp_y = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: self.dst_w as f32,
                Height: self.dst_h as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            ctx.OMSetRenderTargets(Some(&[Some(t.y_rtv.clone())]), None);
            ctx.RSSetViewports(Some(&[vp_y]));
            ctx.PSSetShader(&self.ps_y, None);
            ctx.Draw(3, 0);

            // Chroma, half resolution in both axes.
            let vp_uv = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: (self.dst_w / 2) as f32,
                Height: (self.dst_h / 2) as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            ctx.OMSetRenderTargets(Some(&[Some(t.uv_rtv.clone())]), None);
            ctx.RSSetViewports(Some(&[vp_uv]));
            ctx.PSSetShader(&self.ps_uv, None);
            ctx.Draw(3, 0);

            // Unbind so the encoder can read the texture.
            ctx.OMSetRenderTargets(None, None);
            ctx.PSSetShaderResources(0, Some(&[None]));
            // Submit GPU work before another context (the encoder's) reads it.
            ctx.Flush();
        }
        Ok((t.texture.clone(), slot))
    }

    pub fn has_free_slot(&self) -> bool {
        !self.free_slots.is_empty()
    }

    /// Return a texture slot after the encoder has produced its output.
    pub fn release(&mut self, slot: usize) {
        if slot < self.targets.len() && !self.free_slots.contains(&slot) {
            self.free_slots.push_back(slot);
        }
    }
}

fn create_target(device: &ID3D11Device, w: u32, h: u32) -> Result<Target, String> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        // Encoders commonly sample the surface while reading it.
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32 | D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .map_err(|e| format!("CreateTexture2D NV12 {w}x{h}: {e}"))?;
    let texture = texture.ok_or("NV12 texture null")?;

    // Plane-sliced RTVs for NV12 need the D3D11.3 device interface.
    let device3: ID3D11Device3 = device.cast().map_err(|e| format!("ID3D11Device3: {e}"))?;
    let y_desc = D3D11_RENDER_TARGET_VIEW_DESC1 {
        Format: DXGI_FORMAT_R8_UNORM,
        ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_RENDER_TARGET_VIEW_DESC1_0 {
            Texture2D: D3D11_TEX2D_RTV1 {
                MipSlice: 0,
                PlaneSlice: 0,
            },
        },
    };
    let uv_desc = D3D11_RENDER_TARGET_VIEW_DESC1 {
        Format: DXGI_FORMAT_R8G8_UNORM,
        ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_RENDER_TARGET_VIEW_DESC1_0 {
            Texture2D: D3D11_TEX2D_RTV1 {
                MipSlice: 0,
                PlaneSlice: 1,
            },
        },
    };
    let mut y_rtv1: Option<ID3D11RenderTargetView1> = None;
    let mut uv_rtv1: Option<ID3D11RenderTargetView1> = None;
    unsafe {
        device3
            .CreateRenderTargetView1(&texture, Some(&y_desc), Some(&mut y_rtv1))
            .map_err(|e| format!("CreateRenderTargetView1 Y: {e}"))?;
        device3
            .CreateRenderTargetView1(&texture, Some(&uv_desc), Some(&mut uv_rtv1))
            .map_err(|e| format!("CreateRenderTargetView1 UV: {e}"))?;
    }
    Ok(Target {
        texture,
        y_rtv: y_rtv1
            .ok_or("Y RTV null")?
            .cast()
            .map_err(|e| e.to_string())?,
        uv_rtv: uv_rtv1
            .ok_or("UV RTV null")?
            .cast()
            .map_err(|e| e.to_string())?,
    })
}

fn compile_shader<T: ShaderKind>(
    device: &ID3D11Device,
    src: &str,
    entry: &str,
    target: &str,
) -> Result<T, String> {
    let mut blob: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let entry_c = std::ffi::CString::new(entry).unwrap();
    let target_c = std::ffi::CString::new(target).unwrap();
    unsafe {
        D3DCompile(
            src.as_ptr() as *const _,
            src.len(),
            PCSTR(b"Cliffy\0".as_ptr()),
            None,
            None,
            PCSTR(entry_c.as_ptr() as *const u8),
            PCSTR(target_c.as_ptr() as *const u8),
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut blob as *mut Option<ID3DBlob>,
            Some(&mut errors as *mut Option<ID3DBlob>),
        )
        .map_err(|e| {
            let detail = errors
                .map(|b| {
                    String::from_utf8_lossy(std::slice::from_raw_parts(
                        b.GetBufferPointer() as *const u8,
                        b.GetBufferSize(),
                    ))
                    .to_string()
                })
                .unwrap_or_default();
            format!("shader compile failed ({target}): {e} {detail}")
        })?;
    }
    let blob = blob.ok_or("empty shader blob")?;
    let bytes = unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize())
    };
    unsafe { T::create(device, bytes) }
}

trait ShaderKind: Sized {
    unsafe fn create(device: &ID3D11Device, bytes: &[u8]) -> Result<Self, String>;
}

impl ShaderKind for ID3D11VertexShader {
    unsafe fn create(device: &ID3D11Device, bytes: &[u8]) -> Result<Self, String> {
        let mut out: Option<ID3D11VertexShader> = None;
        device
            .CreateVertexShader(bytes, None, Some(&mut out))
            .map_err(|e| format!("CreateVertexShader: {e}"))?;
        out.ok_or_else(|| "vertex shader null".into())
    }
}

impl ShaderKind for ID3D11PixelShader {
    unsafe fn create(device: &ID3D11Device, bytes: &[u8]) -> Result<Self, String> {
        let mut out: Option<ID3D11PixelShader> = None;
        device
            .CreatePixelShader(bytes, None, Some(&mut out))
            .map_err(|e| format!("CreatePixelShader: {e}"))?;
        out.ok_or_else(|| "pixel shader null".into())
    }
}
