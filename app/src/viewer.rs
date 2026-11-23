//! GPU viewer: an egui_wgpu paint callback that uploads the decoded frame (RGBA f16) and
//! the LUT, then runs viewer.wgsl. Frames are uploaded only when they change.

use eframe::egui_wgpu::{self, wgpu};
use rf_color::{view, CubeLut, ViewSettings};
use rf_core::DecodedFrame;
use std::sync::Arc;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    quad: [f32; 4],
    m0: [f32; 4],
    m1: [f32; 4],
    m2: [f32; 4],
    params: [f32; 4],
    modes: [u32; 4],
    flags: [u32; 4],
    dmin: [f32; 4],
    dmax: [f32; 4],
    d1min: [f32; 4],
    d1max: [f32; 4],
    ap: [f32; 4],
    am: [u32; 4],
}

pub struct ViewerResources {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    ubuf: wgpu::Buffer,
    src: Option<(wgpu::Texture, u32, u32)>,
    src_id: usize,
    lut3: wgpu::Texture,
    lut1: wgpu::Texture,
    lut_id: usize,
    lut3_size: u32,
    lut1_size: u32,
    bind: Option<wgpu::BindGroup>,
    srgb_target: bool,
}

fn tex(device: &wgpu::Device, label: &str, dim: wgpu::TextureDimension, size: wgpu::Extent3d, format: wgpu::TextureFormat) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: dim,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

pub fn init(rs: &egui_wgpu::RenderState) {
    log::debug!("viewer target format: {:?} (adapter: {:?})", rs.target_format, rs.adapter.get_info().name);
    let res = ViewerResources::new(&rs.device, rs.target_format);
    rs.renderer.write().callback_resources.insert(res);
}

impl ViewerResources {
    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("viewer"),
        source: wgpu::ShaderSource::Wgsl(include_str!("viewer.wgsl").into()),
    });
    let entry = |binding: u32, ty: wgpu::BindingType| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
        ty,
        count: None,
    };
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("viewer"),
        entries: &[
            entry(0, wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None }),
            entry(1, wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Float { filterable: true }, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false }),
            entry(2, wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering)),
            entry(3, wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Float { filterable: false }, view_dimension: wgpu::TextureViewDimension::D3, multisampled: false }),
            entry(4, wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Float { filterable: false }, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false }),
        ],
    });
    let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("viewer"),
        bind_group_layouts: &[Some(&layout)],
        immediate_size: 0,
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("viewer"),
        layout: Some(&pl),
        vertex: wgpu::VertexState { module: &shader, entry_point: Some("vs_main"), buffers: &[], compilation_options: Default::default() },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            targets: &[Some(target_format.into())],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("viewer"),
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    let ubuf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("viewer uniforms"),
        size: std::mem::size_of::<Uniforms>() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let one = wgpu::Extent3d { width: 2, height: 2, depth_or_array_layers: 2 };
    let lut3 = tex(device, "lut3", wgpu::TextureDimension::D3, one, wgpu::TextureFormat::Rgba32Float);
    let lut1 = tex(device, "lut1", wgpu::TextureDimension::D2, wgpu::Extent3d { width: 2, height: 1, depth_or_array_layers: 1 }, wgpu::TextureFormat::Rgba32Float);
    ViewerResources {
        pipeline,
        layout,
        sampler,
        ubuf,
        src: None,
        src_id: 0,
        lut3,
        lut1,
        lut_id: 0,
        lut3_size: 0,
        lut1_size: 0,
        bind: None,
        srgb_target: target_format.is_srgb(),
    }
    }
}

/// Everything one paint needs.
pub struct ViewerCallback {
    pub frame: Arc<DecodedFrame>,
    pub view: ViewSettings,
    pub lut: Option<Arc<CubeLut>>,
    /// Image rect within the callback rect, in normalised [0,1] coordinates (x0, y0, x1, y1).
    pub image_uv_rect: [f32; 4],
    /// Focus peaking / false color parameters (viewer-only).
    pub assist: crate::assists::AssistGpu,
}

impl egui_wgpu::CallbackTrait for ViewerCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen: &egui_wgpu::ScreenDescriptor,
        _enc: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(r) = resources.get_mut::<ViewerResources>() else { return vec![] };
        let f = &self.frame;
        let mut rebind = r.bind.is_none();

        // Source frame (pointer identity = frame identity).
        let fid = Arc::as_ptr(&self.frame) as usize;
        if fid != r.src_id {
            if let rf_core::Pixels::RgbaF16(px) = &f.pixels {
                let need_new = !matches!(&r.src, Some((_, w, h)) if *w == f.width && *h == f.height);
                if need_new {
                    let t = tex(
                        device,
                        "frame",
                        wgpu::TextureDimension::D2,
                        wgpu::Extent3d { width: f.width, height: f.height, depth_or_array_layers: 1 },
                        wgpu::TextureFormat::Rgba16Float,
                    );
                    r.src = Some((t, f.width, f.height));
                    rebind = true;
                }
                let (t, _, _) = r.src.as_ref().unwrap();
                queue.write_texture(
                    t.as_image_copy(),
                    bytemuck::cast_slice(px),
                    wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(f.width * 8), rows_per_image: Some(f.height) },
                    wgpu::Extent3d { width: f.width, height: f.height, depth_or_array_layers: 1 },
                );
                r.src_id = fid;
            }
        }

        // LUT
        let lid = self.lut.as_ref().map(|l| Arc::as_ptr(l) as usize).unwrap_or(0);
        if lid != r.lut_id {
            r.lut_id = lid;
            r.lut3_size = 0;
            r.lut1_size = 0;
            if let Some(lut) = &self.lut {
                if let Some(l3) = &lut.lut3d {
                    let n = l3.size as u32;
                    let data: Vec<f32> = l3.data.iter().flat_map(|c| [c[0], c[1], c[2], 1.0]).collect();
                    r.lut3 = tex(device, "lut3", wgpu::TextureDimension::D3, wgpu::Extent3d { width: n, height: n, depth_or_array_layers: n }, wgpu::TextureFormat::Rgba32Float);
                    queue.write_texture(
                        r.lut3.as_image_copy(),
                        bytemuck::cast_slice(&data),
                        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(n * 16), rows_per_image: Some(n) },
                        wgpu::Extent3d { width: n, height: n, depth_or_array_layers: n },
                    );
                    r.lut3_size = n;
                }
                if let Some(l1) = &lut.lut1d {
                    let n = l1.size as u32;
                    let data: Vec<f32> = l1.data.iter().flat_map(|c| [c[0], c[1], c[2], 1.0]).collect();
                    r.lut1 = tex(device, "lut1", wgpu::TextureDimension::D2, wgpu::Extent3d { width: n, height: 1, depth_or_array_layers: 1 }, wgpu::TextureFormat::Rgba32Float);
                    queue.write_texture(
                        r.lut1.as_image_copy(),
                        bytemuck::cast_slice(&data),
                        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(n * 16), rows_per_image: Some(1) },
                        wgpu::Extent3d { width: n, height: 1, depth_or_array_layers: 1 },
                    );
                    r.lut1_size = n;
                }
            }
            rebind = true;
        }

        if rebind {
            if let Some((t, _, _)) = &r.src {
                let v = |t: &wgpu::Texture| t.create_view(&wgpu::TextureViewDescriptor::default());
                r.bind = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("viewer"),
                    layout: &r.layout,
                    entries: &[
                        wgpu::BindGroupEntry { binding: 0, resource: r.ubuf.as_entire_binding() },
                        wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&v(t)) },
                        wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&r.sampler) },
                        wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&v(&r.lut3)) },
                        wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::TextureView(&v(&r.lut1)) },
                    ],
                }));
            }
        }

        // Uniforms
        let enc = &f.encoding;
        let kind = view::input_kind(enc);
        let m = view::display_matrix(enc, self.view.output).0;
        let lut = self.lut.as_deref().filter(|_| self.view.lut_enabled);
        let lut_on = lut.is_some() && (r.lut3_size >= 2 || r.lut1_size >= 2);
        let (dmin, dmax) = lut.and_then(|l| l.lut3d.as_ref()).map(|l| (l.domain_min, l.domain_max)).unwrap_or(([0.0; 3], [1.0; 3]));
        let (d1min, d1max) = lut.and_then(|l| l.lut1d.as_ref()).map(|l| (l.domain_min, l.domain_max)).unwrap_or(([0.0; 3], [1.0; 3]));
        let q = self.image_uv_rect;
        let row = |i: usize| [m[i][0] as f32, m[i][1] as f32, m[i][2] as f32, 0.0];
        let v4 = |a: [f32; 3]| [a[0], a[1], a[2], 0.0];
        let u = Uniforms {
            // uv rect -> NDC (y up)
            quad: [q[0] * 2.0 - 1.0, 1.0 - q[1] * 2.0, q[2] * 2.0 - 1.0, 1.0 - q[3] * 2.0],
            m0: row(0),
            m1: row(1),
            m2: row(2),
            params: [2f32.powf(self.view.exposure_ev), self.view.lut_mix, r.lut3_size as f32, r.lut1_size as f32],
            modes: [
                kind as u32,
                self.view.output.id(),
                lut_on as u32,
                matches!(self.view.lut_placement, rf_color::LutPlacement::AfterCst) as u32,
            ],
            flags: [(self.view.tonemap == rf_color::ToneMap::Rolloff) as u32, (lut_on && r.lut1_size >= 2) as u32, r.srgb_target as u32, 0],
            dmin: v4(dmin),
            dmax: v4(dmax),
            d1min: v4(d1min),
            d1max: v4(d1max),
            ap: self.assist.peak,
            am: self.assist.modes,
        };
        queue.write_buffer(&r.ubuf, 0, bytemuck::bytes_of(&u));
        vec![]
    }

    fn paint(&self, _info: egui::PaintCallbackInfo, pass: &mut wgpu::RenderPass<'static>, resources: &egui_wgpu::CallbackResources) {
        let Some(r) = resources.get::<ViewerResources>() else { return };
        let Some(bind) = &r.bind else { return };
        pass.set_pipeline(&r.pipeline);
        pass.set_bind_group(0, bind, &[]);
        pass.draw(0..6, 0..1);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    //! GPU/CPU parity: the WGSL viewer must produce the same pixels as rf_color's CPU
    //! pipeline (which is what thumbnails and the unit-tested math use).
    use super::*;
    use egui_wgpu::CallbackTrait;
    use rf_color::{view::ViewPipeline, LutPlacement};
    use rf_core::{ColorEncoding, Gamut, Pixels, Transfer};

    const W: u32 = 64;

    pub fn device() -> Option<(wgpu::Device, wgpu::Queue)> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).ok()?;
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).ok()
    }

    fn frame() -> DecodedFrame {
        let mut px = Vec::new();
        for y in 0..W {
            for x in 0..W {
                // Film Gen 5 shadows with varied chroma (where the bug showed), plus a full-range band.
                let (r, g, b) = if y < 48 {
                    let s = |v: u32| 0.09 + 0.14 * v as f32 / 63.0;
                    (s(x), s(y), s((x * 7 + y * 13) % 64))
                } else {
                    let s = |v: u32| v as f32 / 63.0;
                    (s(x), s((x * 3 + y) % 64), s(63 - x))
                };
                px.extend([r, g, b, 1.0].map(half::f16::from_f32));
            }
        }
        DecodedFrame {
            frame: 0,
            width: W,
            height: W,
            pixels: Pixels::RgbaF16(px),
            encoding: ColorEncoding { transfer: Transfer::BmdFilmGen5, gamut: Gamut::BmdWideGamut },
        }
    }

    fn synthetic_lut() -> CubeLut {
        let n = 33;
        let mut s = format!("LUT_3D_SIZE {n}\n");
        for b in 0..n {
            for g in 0..n {
                for r in 0..n {
                    let (r, g, b) = (r as f32 / 32.0, g as f32 / 32.0, b as f32 / 32.0);
                    // Non-separable, clipped near black like a real show LUT.
                    let o = [(r * 1.4 - g * 0.3 - 0.1).clamp(0.0, 1.0), (g * g * 1.2 - b * 0.1).clamp(0.0, 1.0), (b.sqrt() * 0.9 - r * 0.2).clamp(0.0, 1.0)];
                    s += &format!("{} {} {}\n", o[0], o[1], o[2]);
                }
            }
        }
        CubeLut::parse(&s).unwrap()
    }

    pub fn render_uv(f: &Arc<DecodedFrame>, view: ViewSettings, lut: Option<Arc<CubeLut>>, uv: [f32; 4]) -> Option<Vec<u8>> {
        let (dev, q) = device()?;
        Some(render_with(&dev, &q, f, view, lut, uv))
    }

    fn render(dev: &wgpu::Device, q: &wgpu::Queue, f: &Arc<DecodedFrame>, view: ViewSettings, lut: Option<Arc<CubeLut>>) -> Vec<u8> {
        render_with(dev, q, f, view, lut, [0.0, 0.0, 1.0, 1.0])
    }

    fn render_with(dev: &wgpu::Device, q: &wgpu::Queue, f: &Arc<DecodedFrame>, view: ViewSettings, lut: Option<Arc<CubeLut>>, uv: [f32; 4]) -> Vec<u8> {
        render_sized(dev, q, f, view, lut, uv, W, W)
    }

    pub fn render_sized(dev: &wgpu::Device, q: &wgpu::Queue, f: &Arc<DecodedFrame>, view: ViewSettings, lut: Option<Arc<CubeLut>>, uv: [f32; 4], tw: u32, th: u32) -> Vec<u8> {
        
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let mut resources = egui_wgpu::CallbackResources::default();
        resources.insert(ViewerResources::new(dev, format));
        let cb = ViewerCallback { frame: f.clone(), view, lut, image_uv_rect: uv, assist: Default::default() };
        let target = dev.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d { width: tw, height: th, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let tv = target.create_view(&Default::default());
        let mut enc = dev.create_command_encoder(&Default::default());
        let sd = egui_wgpu::ScreenDescriptor { size_in_pixels: [tw, th], pixels_per_point: 1.0 };
        cb.prepare(dev, q, &sd, &mut enc, &mut resources);
        {
            let r = resources.get::<ViewerResources>().unwrap();
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &tv,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&r.pipeline);
            pass.set_bind_group(0, r.bind.as_ref().unwrap(), &[]);
            pass.draw(0..6, 0..1);
        }
        let buf = dev.create_buffer(&wgpu::BufferDescriptor { label: None, size: (tw * th * 4) as u64, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
        enc.copy_texture_to_buffer(
            target.as_image_copy(),
            wgpu::TexelCopyBufferInfo { buffer: &buf, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(tw * 4), rows_per_image: Some(th) } },
            wgpu::Extent3d { width: tw, height: th, depth_or_array_layers: 1 },
        );
        q.submit([enc.finish()]);
        buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        dev.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let out = buf.slice(..).get_mapped_range().unwrap().to_vec();
        out
    }

    fn compare(view: ViewSettings, lut: Option<CubeLut>, name: &str) {
        let Some((dev, q)) = device() else {
            eprintln!("no GPU adapter; skipping {name}");
            return;
        };
        let f = Arc::new(frame());
        let lut = lut.map(Arc::new);
        let gpu = render(&dev, &q, &f, view, lut.clone());
        let cpu = ViewPipeline::new(view, &f.encoding, lut.as_deref());
        let Pixels::RgbaF16(px) = &f.pixels else { unreachable!() };
        let mut worst = (0i32, 0usize);
        for i in 0..(W * W) as usize {
            let v = [px[i * 4].to_f32(), px[i * 4 + 1].to_f32(), px[i * 4 + 2].to_f32()];
            let o = cpu.apply(v);
            for c in 0..3 {
                let d = (gpu[i * 4 + c] as i32 - (o[c] * 255.0 + 0.5) as i32).abs();
                if d > worst.0 {
                    worst = (d, i);
                }
            }
        }
        let i = worst.1;
        assert!(
            worst.0 <= 2,
            "{name}: GPU differs from CPU by {} at pixel {} (input {:?}, gpu {:?}, cpu {:?})",
            worst.0,
            i,
            [px[i * 4].to_f32(), px[i * 4 + 1].to_f32(), px[i * 4 + 2].to_f32()],
            &gpu[i * 4..i * 4 + 3],
            cpu.apply([px[i * 4].to_f32(), px[i * 4 + 1].to_f32(), px[i * 4 + 2].to_f32()]).map(|c| (c * 255.0 + 0.5) as u8)
        );
    }

    #[test]
    fn cst_matches_cpu() {
        compare(ViewSettings { lut_enabled: false, ..Default::default() }, None, "cst");
    }

    #[test]
    fn lut_on_log_matches_cpu() {
        compare(ViewSettings::default(), Some(synthetic_lut()), "lut-on-log");
    }

    #[test]
    fn lut_after_cst_matches_cpu() {
        compare(ViewSettings { lut_placement: LutPlacement::AfterCst, ..Default::default() }, Some(synthetic_lut()), "lut-after-cst");
    }

    #[test]
    fn bmd_lut_matches_cpu() {
        let p = std::path::Path::new(r"C:\ProgramData\Blackmagic Design\DaVinci Resolve\Support\LUT\Blackmagic Design\Blackmagic Gen 5 Film to Extended Video.cube");
        if let Ok(l) = CubeLut::load(p) {
            compare(ViewSettings::default(), Some(l), "bmd-lut");
        }
    }
}

#[cfg(test)]
mod real_clip {
    use super::tests_support::*;
    #[test]
    fn real_frame_lut_matches_cpu() {
        run_real();
    }
}

#[cfg(test)]
mod tests_support {
    use super::*;
    use rf_color::view::ViewPipeline;
    use rf_core::{DecodeRequest, MediaSource, PixelFormat, Pixels, ResolutionScale};

    pub fn run_real() {
        let clip = std::path::Path::new(r"D:\Filmmaking\Before Japan\A002_05302106_C014.braw");
        let lutp = std::path::Path::new(r"C:\ProgramData\Blackmagic Design\DaVinci Resolve\Support\LUT\Blackmagic Design\Blackmagic Gen 5 Film to Extended Video.cube");
        if !clip.exists() || !lutp.exists() {
            return;
        }
        let src = rf_braw::BrawSource::open(clip).unwrap();
        let f = Arc::new(src.decode(&DecodeRequest { frame: 65, scale: ResolutionScale::Full, format: PixelFormat::RgbaF16, color: Default::default() }).unwrap());
        let lut = Arc::new(CubeLut::load(lutp).unwrap());
        let view = ViewSettings::default();
        let (x0, y0) = (3870.0f32, 1500.0f32);
        let w = 64.0;
        let uv = [-x0 / w, -y0 / w, (f.width as f32 - x0) / w, (f.height as f32 - y0) / w];
        let gpu = super::tests::render_uv(&f, view, Some(lut.clone()), uv);
        let Some(gpu) = gpu else { return };
        let cpu = ViewPipeline::new(view, &f.encoding, Some(&lut));
        let Pixels::RgbaF16(px) = &f.pixels else { unreachable!() };
        let mut bad = 0;
        let mut first = None;
        for y in 0..64usize {
            for x in 0..64usize {
                let i = (y0 as usize + y) * f.width as usize + x0 as usize + x;
                let v = [px[i * 4].to_f32(), px[i * 4 + 1].to_f32(), px[i * 4 + 2].to_f32()];
                let o = cpu.apply(v).map(|c| (c * 255.0 + 0.5) as i32);
                let g = &gpu[(y * 64 + x) * 4..(y * 64 + x) * 4 + 3];
                if (0..3).any(|c| (g[c] as i32 - o[c]).abs() > 2) {
                    bad += 1;
                    first.get_or_insert((v, g.to_vec(), o));
                }
            }
        }
        assert_eq!(bad, 0, "{bad} pixels differ; first: {first:?}");
    }
}

#[cfg(test)]
mod fit_downscale {
    use super::*;
    use rf_core::{DecodeRequest, MediaSource, PixelFormat, ResolutionScale};
    #[test]
    fn fit_render_has_no_black_specks() {
        let clip = std::path::Path::new(r"D:\Filmmaking\Before Japan\A002_05302106_C014.braw");
        let lutp = std::path::Path::new(r"C:\ProgramData\Blackmagic Design\DaVinci Resolve\Support\LUT\Blackmagic Design\Blackmagic Gen 5 Film to Extended Video.cube");
        if !clip.exists() || !lutp.exists() {
            return;
        }
        let Some((dev, q)) = super::tests::device() else { return };
        let src = rf_braw::BrawSource::open(clip).unwrap();
        let f = Arc::new(src.decode(&DecodeRequest { frame: 65, scale: ResolutionScale::Full, format: PixelFormat::RgbaF16, color: Default::default() }).unwrap());
        let lut = Some(Arc::new(CubeLut::load(lutp).unwrap()));
        for (name, view) in [("lut", ViewSettings::default()), ("cst", ViewSettings { lut_enabled: false, ..Default::default() })] {
            let out = super::tests::render_sized(&dev, &q, &f, view, lut.clone(), [0.0, 0.0, 1.0, 1.0], 1088, 574);
            let zeros = out.chunks(4).filter(|p| p[0] == 0 && p[1] == 0 && p[2] == 0).count();
            let dark = out.chunks(4).filter(|p| p[0].max(p[1]).max(p[2]) <= 8).count();
            let hist: Vec<usize> = (0..12u8).map(|v| out.chunks(4).filter(|p| p[0].max(p[1]).max(p[2]) == v).count()).collect();
            eprintln!("{name}: pixels with every channel <= 8/255 = {dark}; max-channel histogram 0..11 = {hist:?}");
            if let Ok(dir) = std::env::var("RF_REVIEW_OUT") {
                // Evidence for review: the GPU's own output, read back from the render target.
                let mut ppm = format!("P6\n1088 574\n255\n").into_bytes();
                ppm.extend(out.chunks(4).flat_map(|p| [p[0], p[1], p[2]]));
                std::fs::write(format!("{dir}/gpu_readback_{name}.ppm"), ppm).unwrap();
            }
            eprintln!("{name}: black pixels at fit = {zeros}");
            assert_eq!(zeros, 0, "{name}: {zeros} black pixels when downscaled");
        }
    }
}

#[cfg(test)]
mod app_geometry {
    use super::*;
    use rf_core::{DecodeRequest, MediaSource, PixelFormat, ResolutionScale};
    #[test]
    fn app_geometry_lut_render_has_no_black_specks() {
        let clip = std::path::Path::new(r"D:\Filmmaking\Before Japan\A002_05302106_C014.braw");
        let lutp = std::path::Path::new(r"C:\ProgramData\Blackmagic Design\DaVinci Resolve\Support\LUT\Blackmagic Design\Blackmagic Gen 5 Film to Extended Video.cube");
        if !clip.exists() || !lutp.exists() {
            return;
        }
        let Some((dev, q)) = super::tests::device() else { return };
        let src = rf_braw::BrawSource::open(clip).unwrap();
        let f = Arc::new(src.decode(&DecodeRequest { frame: 65, scale: ResolutionScale::Full, format: PixelFormat::RgbaF16, color: Default::default() }).unwrap());
        let lut = Some(Arc::new(CubeLut::load(lutp).unwrap()));
        let (tw, th) = (1152u32, 672u32);
        let (x0, x1) = (40.0f32, 1112.0f32);
        let h = 1072.0 * 2160.0 / 4096.0;
        let y0 = (th as f32 - h) / 2.0;
        let uv = [x0 / tw as f32, y0 / th as f32, x1 / tw as f32, (y0 + h) / th as f32];
        let out = super::tests::render_sized(&dev, &q, &f, ViewSettings::default(), lut, uv, tw, th);
        let mut zeros = 0;
        for y in (y0.ceil() as u32 + 1)..((y0 + h) as u32 - 1) {
            for x in 41..1111u32 {
                let i = ((y * tw + x) * 4) as usize;
                if out[i] == 0 && out[i + 1] == 0 && out[i + 2] == 0 {
                    zeros += 1;
                }
            }
        }
        eprintln!("app geometry: black pixels = {zeros}");
        assert_eq!(zeros, 0);
    }
}
