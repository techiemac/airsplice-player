//! AMD FSR 1 (EASU upscaling, then RCAS sharpening) as wgpu compute
//! shaders, for any GPU wgpu can drive: Vulkan (Linux, Windows, Raspberry
//! Pi), Direct3D 12 and Metal.

use std::sync::{Arc, Mutex, OnceLock, mpsc};

use anyhow::{Context as _, Result, anyhow, bail};

use super::Upscaler;

/// RCAS strength in stops (0 is the sharpest); AMD's suggested default.
const SHARPNESS_STOPS: f32 = 0.2;
const WORKGROUP: u32 = 8;

/// The GPU and compiled shaders, shared by every video thread.
struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    layout: wgpu::BindGroupLayout,
    easu: wgpu::ComputePipeline,
    rcas: wgpu::ComputePipeline,
    name: String,
    /// The last validation or device error, from wgpu's error callback.
    error: Arc<Mutex<Option<String>>>,
}

fn gpu(allow_software: bool) -> Option<&'static Gpu> {
    static HARDWARE: OnceLock<Option<Gpu>> = OnceLock::new();
    static ANY: OnceLock<Option<Gpu>> = OnceLock::new();
    let cell = if allow_software { &ANY } else { &HARDWARE };
    cell.get_or_init(|| match Gpu::new(allow_software) {
        Ok(gpu) => Some(gpu),
        Err(e) => {
            log::info!("FSR upscaling unavailable: {e:#}");
            None
        }
    })
    .as_ref()
}

impl Gpu {
    fn new(allow_software: bool) -> Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor { backends: wgpu::Backends::PRIMARY, ..wgpu::InstanceDescriptor::new_without_display_handle() });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))
        .context("no GPU adapter")?;
        let info = adapter.get_info();
        // A software rasteriser (llvmpipe, WARP) is far slower than the
        // display's own bilinear scaling; only tests use one.
        if info.device_type == wgpu::DeviceType::Cpu && !allow_software {
            bail!("only a software renderer ({}) is available", info.name);
        }
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("upscale"),
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            ..Default::default()
        }))
        .context("creating the GPU device")?;
        let error = Arc::new(Mutex::new(None));
        device.on_uncaptured_error(Arc::new({
            let error = error.clone();
            move |e: wgpu::Error| *error.lock().unwrap() = Some(e.to_string())
        }));

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("fsr1"),
            source: wgpu::ShaderSource::Wgsl(include_str!("fsr1.wgsl").into()),
        });
        let entry = |binding, ty| wgpu::BindGroupLayoutEntry { binding, visibility: wgpu::ShaderStages::COMPUTE, ty, count: None };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("fsr1"),
            entries: &[
                entry(0, wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                }),
                entry(1, wgpu::BindingType::StorageTexture {
                    access: wgpu::StorageTextureAccess::WriteOnly,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    view_dimension: wgpu::TextureViewDimension::D2,
                }),
                entry(2, wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None }),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("fsr1"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = |entry_point: &str| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry_point),
                layout: Some(&pipeline_layout),
                module: &shader,
                entry_point: Some(entry_point),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        let (easu, rcas) = (pipeline("easu"), pipeline("rcas"));
        if let Some(e) = error.lock().unwrap().take() {
            bail!("compiling the FSR shaders: {e}");
        }
        let backend = match info.backend {
            wgpu::Backend::Vulkan => "Vulkan",
            wgpu::Backend::Metal => "Metal",
            wgpu::Backend::Dx12 => "Direct3D 12",
            _ => "GPU",
        };
        log::info!("FSR upscaling on {} ({backend})", info.name.trim());
        let name = "AMD FSR 1".to_string();
        Ok(Self { device, queue, layout, easu, rcas, name, error })
    }
}

/// Textures and buffers for one input and output size.
struct Frames {
    size: (u32, u32, u32, u32),
    input: wgpu::Texture,
    output: wgpu::Texture,
    readback: wgpu::Buffer,
    padded_row: u32,
    easu: wgpu::BindGroup,
    rcas: wgpu::BindGroup,
}

pub struct Fsr {
    gpu: &'static Gpu,
    frames: Option<Frames>,
}

impl Fsr {
    /// FSR on the best GPU, or `None` without one. `allow_software` admits
    /// a CPU rasteriser, for tests on machines without a GPU.
    pub fn new(allow_software: bool) -> Option<Self> {
        Some(Self { gpu: gpu(allow_software)?, frames: None })
    }

    pub fn name(&self) -> String {
        self.gpu.name.clone()
    }

    fn frames(&mut self, w: u32, h: u32, ow: u32, oh: u32) -> &Frames {
        if self.frames.as_ref().is_none_or(|f| f.size != (w, h, ow, oh)) {
            self.frames = Some(self.create_frames(w, h, ow, oh));
        }
        self.frames.as_ref().unwrap()
    }

    fn create_frames(&self, w: u32, h: u32, ow: u32, oh: u32) -> Frames {
        let device = &self.gpu.device;
        let texture = |label, width, height, usage| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage,
                view_formats: &[],
            })
        };
        use wgpu::TextureUsages as U;
        let input = texture("input", w, h, U::TEXTURE_BINDING | U::COPY_DST);
        let middle = texture("easu", ow, oh, U::TEXTURE_BINDING | U::STORAGE_BINDING);
        let output = texture("rcas", ow, oh, U::STORAGE_BINDING | U::COPY_SRC);
        let mut params = Vec::with_capacity(32);
        for v in [w, h, ow, oh] {
            params.extend_from_slice(&v.to_le_bytes());
        }
        for v in [(-SHARPNESS_STOPS).exp2(), 0.0, 0.0, 0.0] {
            params.extend_from_slice(&v.to_le_bytes());
        }
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("fsr1 params"),
            size: params.len() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.gpu.queue.write_buffer(&uniform, 0, &params);
        let padded_row = (ow * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: padded_row as u64 * oh as u64,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let view = |t: &wgpu::Texture| t.create_view(&Default::default());
        let bind = |from: &wgpu::Texture, to: &wgpu::Texture| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("fsr1"),
                layout: &self.gpu.layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view(from)) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&view(to)) },
                    wgpu::BindGroupEntry { binding: 2, resource: uniform.as_entire_binding() },
                ],
            })
        };
        let (easu, rcas) = (bind(&input, &middle), bind(&middle, &output));
        Frames { size: (w, h, ow, oh), input, output, readback, padded_row, easu, rcas }
    }
}

impl Upscaler for Fsr {
    fn upscale(&mut self, bgra: &[u8], w: u32, h: u32, ow: u32, oh: u32) -> Result<Vec<u8>> {
        anyhow::ensure!(bgra.len() == (w * h * 4) as usize, "frame is {} bytes, expected {w}x{h}x4", bgra.len());
        let gpu = self.gpu;
        let frames = self.frames(w, h, ow, oh);
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: &frames.input, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            bgra,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_compute_pass(&Default::default());
            let groups = (ow.div_ceil(WORKGROUP), oh.div_ceil(WORKGROUP));
            for (pipeline, bind) in [(&gpu.easu, &frames.easu), (&gpu.rcas, &frames.rcas)] {
                pass.set_pipeline(pipeline);
                pass.set_bind_group(0, bind, &[]);
                pass.dispatch_workgroups(groups.0, groups.1, 1);
            }
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &frames.output, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo {
                buffer: &frames.readback,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(frames.padded_row), rows_per_image: Some(oh) },
            },
            wgpu::Extent3d { width: ow, height: oh, depth_or_array_layers: 1 },
        );
        gpu.queue.submit([encoder.finish()]);

        let slice = frames.readback.slice(..);
        let (tx, rx) = mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        gpu.device.poll(wgpu::PollType::wait_indefinitely()).map_err(|e| anyhow!("waiting for the GPU: {e}"))?;
        if let Some(e) = gpu.error.lock().unwrap().take() {
            bail!("GPU error: {e}");
        }
        rx.recv().context("GPU readback never completed")?.map_err(|e| anyhow!("reading the frame back: {e}"))?;
        let mut out = vec![0u8; (ow * oh * 4) as usize];
        {
            let mapped = slice.get_mapped_range().map_err(|e| anyhow!("mapping the frame: {e:?}"))?;
            let row = (ow * 4) as usize;
            for (dst, src) in out.chunks_exact_mut(row).zip(mapped.chunks(frames.padded_row as usize)) {
                dst.copy_from_slice(&src[..row]);
            }
        }
        frames.readback.unmap();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FSR on whatever GPU this machine has, a software one included (CI
    /// and Docker have none). Skips, saying so, if there is not even that.
    fn fsr() -> Option<Fsr> {
        let fsr = Fsr::new(true);
        if fsr.is_none() {
            eprintln!("skipping: no GPU or software renderer for wgpu");
        }
        fsr
    }

    #[test]
    fn fsr_restores_detail_better_than_bilinear() {
        let Some(mut fsr) = fsr() else { return };
        eprintln!("{}", fsr.name());
        crate::media::upscale::tests::check_quality(&mut fsr);
    }

    #[test]
    fn fsr_handles_size_changes_and_odd_sizes() {
        let Some(mut fsr) = fsr() else { return };
        for (w, h, ow, oh) in [(33, 17, 61, 31), (16, 16, 32, 32), (33, 17, 50, 26)] {
            let out = fsr.upscale(&vec![128; (w * h * 4) as usize], w, h, ow, oh).unwrap();
            assert_eq!(out.len(), (ow * oh * 4) as usize);
        }
        assert!(fsr.upscale(&[0; 7], 2, 2, 4, 4).is_err(), "a short frame is an error, not a crash");
    }
}
