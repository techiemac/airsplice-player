//! MetalFX spatial upscaling: the system's real-time upscaler on Apple
//! GPUs (Apple silicon, and the AMD GPUs MetalFX supports).

use std::ptr::NonNull;
use std::sync::OnceLock;

use anyhow::{Context as _, Result, bail, ensure};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::ProtocolObject;
use objc2_metal::{
    MTLBlitCommandEncoder, MTLBuffer, MTLCommandBuffer, MTLCommandBufferStatus, MTLCommandEncoder, MTLCommandQueue,
    MTLCreateSystemDefaultDevice, MTLDevice, MTLOrigin, MTLPixelFormat, MTLRegion, MTLResourceOptions, MTLSize, MTLStorageMode,
    MTLTexture, MTLTextureDescriptor,
};
use objc2_metal_fx::{MTLFXSpatialScaler, MTLFXSpatialScalerBase, MTLFXSpatialScalerColorProcessingMode, MTLFXSpatialScalerDescriptor};

use super::Upscaler;

/// The system GPU and a command queue, shared by every video thread.
struct Gpu {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
}

// Metal devices and command queues are documented as safe to use from any
// thread; each video thread makes its own command buffers.
unsafe impl Send for Gpu {}
unsafe impl Sync for Gpu {}

fn gpu() -> Option<&'static Gpu> {
    static GPU: OnceLock<Option<Gpu>> = OnceLock::new();
    GPU.get_or_init(|| {
        let device = MTLCreateSystemDefaultDevice()?;
        // SAFETY: a class method taking a valid device.
        if !unsafe { MTLFXSpatialScalerDescriptor::supportsDevice(&device) } {
            log::info!("MetalFX does not support {}", device.name());
            return None;
        }
        let queue = device.newCommandQueue()?;
        Some(Gpu { device, queue })
    })
    .as_ref()
}

/// The scaler and textures for one input and output size.
struct Frames {
    size: (u32, u32, u32, u32),
    scaler: Retained<ProtocolObject<dyn MTLFXSpatialScaler>>,
    input: Retained<ProtocolObject<dyn MTLTexture>>,
    output: Retained<ProtocolObject<dyn MTLTexture>>,
    readback: Retained<ProtocolObject<dyn MTLBuffer>>,
}

pub struct MetalFx {
    gpu: &'static Gpu,
    frames: Option<Frames>,
}

impl MetalFx {
    pub fn new() -> Option<Self> {
        Some(Self { gpu: gpu()?, frames: None })
    }

    pub fn name(&self) -> String {
        "MetalFX".into()
    }

    fn create_frames(&self, w: u32, h: u32, ow: u32, oh: u32) -> Result<Frames> {
        let device = &self.gpu.device;
        // SAFETY: plain property setters on a fresh descriptor, then the
        // documented factory method with a supported device.
        let scaler = unsafe {
            let desc = MTLFXSpatialScalerDescriptor::new();
            desc.setColorTextureFormat(MTLPixelFormat::BGRA8Unorm);
            desc.setOutputTextureFormat(MTLPixelFormat::BGRA8Unorm);
            desc.setInputWidth(w as usize);
            desc.setInputHeight(h as usize);
            desc.setOutputWidth(ow as usize);
            desc.setOutputHeight(oh as usize);
            // Video is gamma-encoded, which "perceptual" expects.
            desc.setColorProcessingMode(MTLFXSpatialScalerColorProcessingMode::Perceptual);
            desc.newSpatialScalerWithDevice(device)
        }
        .context("MetalFX could not create a scaler for this size")?;
        let texture = |width: u32, height: u32, usage, storage| {
            // SAFETY: a valid pixel format and non-zero size.
            let desc = unsafe {
                MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                    MTLPixelFormat::BGRA8Unorm,
                    width as usize,
                    height as usize,
                    false,
                )
            };
            desc.setUsage(usage);
            desc.setStorageMode(storage);
            device.newTextureWithDescriptor(&desc).context("allocating a texture")
        };
        // The CPU writes the input: shared memory on Apple silicon, a
        // managed copy on a discrete GPU. MetalFX requires private output.
        let upload = if device.hasUnifiedMemory() { MTLStorageMode::Shared } else { MTLStorageMode::Managed };
        // SAFETY: property getters on a valid scaler.
        let (in_usage, out_usage) = unsafe { (scaler.colorTextureUsage(), scaler.outputTextureUsage()) };
        let input = texture(w, h, in_usage, upload)?;
        let output = texture(ow, oh, out_usage, MTLStorageMode::Private)?;
        let readback = device
            .newBufferWithLength_options((ow * oh * 4) as usize, MTLResourceOptions::StorageModeShared)
            .context("allocating the readback buffer")?;
        Ok(Frames { size: (w, h, ow, oh), scaler, input, output, readback })
    }
}

impl Upscaler for MetalFx {
    fn upscale(&mut self, bgra: &[u8], w: u32, h: u32, ow: u32, oh: u32) -> Result<Vec<u8>> {
        ensure!(bgra.len() == (w * h * 4) as usize, "frame is {} bytes, expected {w}x{h}x4", bgra.len());
        if self.frames.as_ref().is_none_or(|f| f.size != (w, h, ow, oh)) {
            self.frames = Some(self.create_frames(w, h, ow, oh)?);
        }
        let frames = self.frames.as_ref().unwrap();
        // Command buffers are autoreleased; video threads have no pool.
        autoreleasepool(|_| {
            let region = MTLRegion { origin: MTLOrigin { x: 0, y: 0, z: 0 }, size: MTLSize { width: w as usize, height: h as usize, depth: 1 } };
            // SAFETY: `bgra` holds exactly `h` rows of `w * 4` bytes.
            unsafe {
                frames.input.replaceRegion_mipmapLevel_withBytes_bytesPerRow(region, 0, NonNull::from(bgra).cast(), (w * 4) as usize);
            }
            let cmd = self.gpu.queue.commandBuffer().context("no command buffer")?;
            // SAFETY: textures of the sizes, formats and usages the scaler
            // was created for.
            unsafe {
                frames.scaler.setColorTexture(Some(&frames.input));
                frames.scaler.setOutputTexture(Some(&frames.output));
                frames.scaler.setInputContentWidth(w as usize);
                frames.scaler.setInputContentHeight(h as usize);
                frames.scaler.encodeToCommandBuffer(&cmd);
            }
            let blit = cmd.blitCommandEncoder().context("no blit encoder")?;
            // SAFETY: the whole output texture into a buffer sized for it.
            unsafe {
                blit.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toBuffer_destinationOffset_destinationBytesPerRow_destinationBytesPerImage(
                    &frames.output,
                    0,
                    0,
                    MTLOrigin { x: 0, y: 0, z: 0 },
                    MTLSize { width: ow as usize, height: oh as usize, depth: 1 },
                    &frames.readback,
                    0,
                    (ow * 4) as usize,
                    (ow * oh * 4) as usize,
                );
            }
            blit.endEncoding();
            cmd.commit();
            cmd.waitUntilCompleted();
            if cmd.status() != MTLCommandBufferStatus::Completed {
                bail!("the GPU did not finish the frame (status {:?})", cmd.status());
            }
            let len = (ow * oh * 4) as usize;
            // SAFETY: the buffer is `len` bytes of shared memory, and the
            // GPU is done writing it.
            let out = unsafe { std::slice::from_raw_parts(frames.readback.contents().as_ptr().cast::<u8>(), len) }.to_vec();
            Ok(out)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metalfx_restores_detail_better_than_bilinear() {
        let Some(mut m) = MetalFx::new() else {
            eprintln!("skipping: MetalFX is not available on this Mac");
            return;
        };
        eprintln!("{}", m.name());
        crate::media::upscale::tests::check_quality(&mut m);
    }
}
