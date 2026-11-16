//! Hardware decoding on macOS through VideoToolbox: H.264, HEVC (8 and 10
//! bit), VP9 and AV1 where the machine supports them. VideoToolbox converts
//! to BGRA on the GPU, so frames arrive ready for display.

use std::ffi::c_void;
use std::ptr::{NonNull, null, null_mut};
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, AtomicI64, Ordering};

use anyhow::{Result, anyhow, bail};
use objc2_core_foundation::{CFData, CFDictionary, CFNumber, CFNumberType, CFRetained, CFString};
use objc2_core_media::{
    CMBlockBuffer, CMSampleBuffer, CMSampleTimingInfo, CMTime, CMTimeFlags, CMVideoCodecType, CMVideoFormatDescription, CMVideoFormatDescriptionCreate,
    kCMBlockBufferAssureMemoryNowFlag, kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms,
    kCMVideoCodecType_AV1, kCMVideoCodecType_H264, kCMVideoCodecType_HEVC, kCMVideoCodecType_VP9,
};
use objc2_core_video::{
    CVImageBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_32BGRA,
};
use objc2_video_toolbox::{
    VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionOutputCallbackRecord, VTDecompressionSession,
    VTIsHardwareDecodeSupported, VTRegisterSupplementalVideoDecoderIfAvailable,
};
use symphonia::core::codecs::video::VideoCodecParameters;
use symphonia::core::codecs::video::well_known::extra_data::{
    VIDEO_EXTRA_DATA_ID_AV1_DECODER_CONFIG, VIDEO_EXTRA_DATA_ID_AVC_DECODER_CONFIG,
    VIDEO_EXTRA_DATA_ID_HEVC_DECODER_CONFIG,
};
use symphonia::core::codecs::video::well_known::{CODEC_ID_AV1, CODEC_ID_H264, CODEC_ID_HEVC, CODEC_ID_VP9};

use super::bitstream::{hevc_reorder_depth, vp9_keyframe, vpcc};
use super::{DecodedFrame, KeyframeCheck, Reorder, VideoDecoder, keyframe_check};

/// Where the output callback leaves frames. Boxed so its address is stable
/// for the callback's refcon.
struct Output {
    frames: Mutex<Vec<DecodedFrame>>,
    discard_before: AtomicI64,
    /// The status of the last frame VideoToolbox could not decode, until
    /// `decode` reports it; 0 for none.
    failed: AtomicI32,
}

pub struct VtDecoder {
    codec: CMVideoCodecType,
    /// The `SampleDescriptionExtensionAtoms` entry: (`avcC`, record).
    /// VP9 builds its `vpcC` from the first keyframe, so it starts empty.
    atom: Option<(&'static str, Vec<u8>)>,
    width: i32,
    height: i32,
    session: Option<(CFRetained<CMVideoFormatDescription>, CFRetained<VTDecompressionSession>)>,
    output: Box<Output>,
    reorder: Reorder,
    is_keyframe: KeyframeCheck,
    seen_keyframe: bool,
}

// SAFETY: the session and format description are only used from the thread
// that owns the decoder; the callback only touches `Output`, which is Sync.
unsafe impl Send for VtDecoder {}

impl VtDecoder {
    pub fn new(params: &VideoCodecParameters) -> Result<Self> {
        let extra = |id| params.extra_data.iter().find(|e| e.id == id).map(|e| e.data.to_vec());
        let (codec, atom) = match params.codec {
            CODEC_ID_H264 => (kCMVideoCodecType_H264, Some(("avcC", extra(VIDEO_EXTRA_DATA_ID_AVC_DECODER_CONFIG)))),
            CODEC_ID_HEVC => (kCMVideoCodecType_HEVC, Some(("hvcC", extra(VIDEO_EXTRA_DATA_ID_HEVC_DECODER_CONFIG)))),
            CODEC_ID_AV1 => (kCMVideoCodecType_AV1, Some(("av1C", extra(VIDEO_EXTRA_DATA_ID_AV1_DECODER_CONFIG)))),
            CODEC_ID_VP9 => (kCMVideoCodecType_VP9, None),
            other => bail!("VideoToolbox: unsupported codec {other:?}"),
        };
        let atom = match atom {
            Some((name, Some(data))) => Some((name, data)),
            Some((name, None)) => bail!("VideoToolbox: track has no {name} record"),
            None => None,
        };
        // SAFETY: plain query/registration calls with a valid codec type.
        let hardware = unsafe {
            if codec == kCMVideoCodecType_VP9 || codec == kCMVideoCodecType_AV1 {
                VTRegisterSupplementalVideoDecoderIfAvailable(codec);
            }
            VTIsHardwareDecodeSupported(codec)
        };
        // H.264 and HEVC always have a VideoToolbox decoder; VP9 and AV1
        // only where the GPU supports them (software fallbacks exist).
        if !hardware && (codec == kCMVideoCodecType_VP9 || codec == kCMVideoCodecType_AV1) {
            bail!("VideoToolbox: no hardware decoder for this codec on this Mac");
        }
        let mut dec = Self {
            codec,
            atom,
            width: params.width.unwrap_or(0) as i32,
            height: params.height.unwrap_or(0) as i32,
            session: None,
            output: Box::new(Output {
                frames: Mutex::new(Vec::new()),
                discard_before: AtomicI64::new(i64::MIN),
                failed: AtomicI32::new(0),
            }),
            // VideoToolbox emits decode order; H.264 and HEVC may reorder.
            reorder: Reorder::new(match params.codec {
                CODEC_ID_H264 => extra(VIDEO_EXTRA_DATA_ID_AVC_DECODER_CONFIG)
                    .and_then(|c| super::h264::parse_avcc(&c).ok())
                    .and_then(|c| super::h264::reorder_depth(&c.sps))
                    .unwrap_or(16),
                CODEC_ID_HEVC => extra(VIDEO_EXTRA_DATA_ID_HEVC_DECODER_CONFIG)
                    .and_then(|c| hevc_reorder_depth(&c))
                    .unwrap_or(16),
                _ => 0,
            }),
            is_keyframe: keyframe_check(params)?,
            seen_keyframe: false,
        };
        if dec.atom.is_some() {
            // Create eagerly so an unusable configuration falls back to the
            // software decoder instead of failing mid-playback.
            dec.open_session()?;
        }
        Ok(dec)
    }

    fn open_session(&mut self) -> Result<()> {
        let (name, record) = self.atom.as_ref().ok_or_else(|| anyhow!("VideoToolbox: no configuration record yet"))?;
        let atoms = CFDictionary::<CFString, CFData>::from_slices(&[&*CFString::from_str(name)], &[&*CFData::from_bytes(record)]);
        // SAFETY: framework constants are valid static CFStrings.
        let atoms_key = unsafe { kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms };
        let extensions = CFDictionary::<CFString, CFDictionary>::from_slices(&[atoms_key], &[atoms.as_opaque()]);

        let mut format: *const CMVideoFormatDescription = null();
        // SAFETY: valid dictionary and out-pointer.
        let status = unsafe {
            CMVideoFormatDescriptionCreate(
                None,
                self.codec,
                self.width,
                self.height,
                Some(extensions.as_opaque()),
                NonNull::from(&mut format),
            )
        };
        let format = NonNull::new(format.cast_mut())
            .filter(|_| status == 0)
            .ok_or_else(|| anyhow!("VideoToolbox: CMVideoFormatDescriptionCreate failed ({status})"))?;
        // SAFETY: a +1 reference returned by a Create function.
        let format = unsafe { CFRetained::from_raw(format) };

        let bgra = kCVPixelFormatType_32BGRA as i32;
        // SAFETY: points at a live i32 of the declared type.
        let bgra = unsafe { CFNumber::new(None, CFNumberType::SInt32Type, (&raw const bgra).cast()) }
            .ok_or_else(|| anyhow!("VideoToolbox: CFNumberCreate failed"))?;
        // SAFETY: framework constant.
        let pixel_format_key = unsafe { kCVPixelBufferPixelFormatTypeKey };
        let attrs = CFDictionary::<CFString, CFNumber>::from_slices(&[pixel_format_key], &[&*bgra]);

        let record = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: Some(on_frame),
            decompressionOutputRefCon: (&raw const *self.output).cast_mut().cast(),
        };
        let mut session: *mut VTDecompressionSession = null_mut();
        // SAFETY: valid format, attributes and callback record; the refcon
        // (`self.output`) outlives the session, which is invalidated in
        // `close_session` before the box is dropped.
        let status = unsafe {
            VTDecompressionSession::create(None, &format, None, Some(attrs.as_opaque()), &record, NonNull::from(&mut session))
        };
        let session = NonNull::new(session)
            .filter(|_| status == 0)
            .ok_or_else(|| anyhow!("VideoToolbox: VTDecompressionSessionCreate failed ({status})"))?;
        // SAFETY: a +1 reference returned by a Create function.
        self.session = Some((format, unsafe { CFRetained::from_raw(session) }));
        Ok(())
    }

    fn close_session(&mut self) {
        if let Some((_, session)) = self.session.take() {
            // SAFETY: valid session; invalidating waits for in-flight frames.
            unsafe { session.invalidate() };
        }
    }

    /// Collect callback output and release whatever is now in order.
    fn collect(&mut self) -> Vec<DecodedFrame> {
        let frames = std::mem::take(&mut *self.output.frames.lock().unwrap());
        self.reorder.push(frames)
    }
}

/// VideoToolbox output callback. `frame_refcon` carries the packet's PTS.
unsafe extern "C-unwind" fn on_frame(
    refcon: *mut c_void,
    frame_refcon: *mut c_void,
    status: i32,
    flags: VTDecodeInfoFlags,
    image: *mut CVImageBuffer,
    _pts: CMTime,
    _duration: CMTime,
) {
    // SAFETY: refcon is the decoder's boxed `Output`, alive while the
    // session is.
    let output = unsafe { &*(refcon as *const Output) };
    let pts = frame_refcon as i64;
    if status != 0 {
        output.failed.store(status, Ordering::Relaxed);
        return;
    }
    if image.is_null() || flags.contains(VTDecodeInfoFlags::FrameDropped) {
        return;
    }
    if pts < output.discard_before.load(Ordering::Relaxed) {
        return;
    }
    // SAFETY: VideoToolbox hands us a valid pixel buffer for the duration
    // of the callback; we lock it read-only while copying.
    let frame = unsafe {
        let buffer = &*image;
        if CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly) != 0 {
            return;
        }
        let (w, h, stride) = (CVPixelBufferGetWidth(buffer), CVPixelBufferGetHeight(buffer), CVPixelBufferGetBytesPerRow(buffer));
        let base = CVPixelBufferGetBaseAddress(buffer) as *const u8;
        let mut bgra = Vec::with_capacity(w * h * 4);
        if !base.is_null() {
            for row in 0..h {
                bgra.extend_from_slice(std::slice::from_raw_parts(base.add(row * stride), w * 4));
            }
        }
        CVPixelBufferUnlockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly);
        if base.is_null() {
            return;
        }
        DecodedFrame { pts, width: w as u32, height: h as u32, bgra }
    };
    output.frames.lock().unwrap().push(frame);
}

impl VideoDecoder for VtDecoder {
    fn name(&self) -> &'static str {
        "VideoToolbox (hardware)"
    }

    fn is_keyframe(&self, data: &[u8]) -> bool {
        (self.is_keyframe)(data)
    }

    fn decode(&mut self, data: &[u8], pts: i64) -> Result<Vec<DecodedFrame>> {
        if !self.seen_keyframe {
            if !self.is_keyframe(data) {
                return Ok(Vec::new());
            }
            self.seen_keyframe = true;
        }
        if self.session.is_none() {
            // VP9: describe the stream from its first keyframe.
            let config = vp9_keyframe(data).ok_or_else(|| anyhow!("VideoToolbox: VP9 stream does not start with a keyframe"))?;
            // VideoToolbox decodes VP9 profiles 0 and 2 (4:2:0) only; it
            // takes the others and returns nothing.
            if config.profile % 2 == 1 {
                let chroma = if config.chroma_subsampling == 2 { "4:2:2" } else { "4:4:4" };
                bail!("VP9 {chroma} video (profile {}) is not supported", config.profile);
            }
            self.atom = Some(("vpcC", vpcc(&config)));
            self.open_session()?;
        }
        let (format, session) = self.session.as_ref().expect("opened above");

        let len = data.len();
        let mut block: *mut CMBlockBuffer = null_mut();
        // SAFETY: CoreMedia allocates `len` bytes; we fill them below.
        let status = unsafe {
            CMBlockBuffer::create_with_memory_block(
                None,
                null_mut(),
                len,
                None,
                null(),
                0,
                len,
                kCMBlockBufferAssureMemoryNowFlag,
                NonNull::from(&mut block),
            )
        };
        let block = NonNull::new(block).filter(|_| status == 0).ok_or_else(|| anyhow!("CMBlockBufferCreate failed ({status})"))?;
        // SAFETY: +1 reference from a Create function.
        let block = unsafe { CFRetained::from_raw(block) };
        // SAFETY: copies `len` bytes from a live slice into the block.
        let status = unsafe { CMBlockBuffer::replace_data_bytes(NonNull::from(data).cast(), &block, 0, len) };
        if status != 0 {
            bail!("CMBlockBufferReplaceDataBytes failed ({status})");
        }
        let mut sample: *mut CMSampleBuffer = null_mut();
        // Presentation time on the sample, for VideoToolbox's bookkeeping;
        // ordering is done by `Reorder`. Raw track ticks, unit timescale.
        let time = |value: i64, flags: CMTimeFlags| CMTime { value, timescale: 1, flags, epoch: 0 };
        let timing = CMSampleTimingInfo {
            duration: time(0, CMTimeFlags::empty()),
            presentationTimeStamp: time(pts, CMTimeFlags::Valid),
            decodeTimeStamp: time(0, CMTimeFlags::empty()),
        };
        // SAFETY: valid block and format; one sample of `len` bytes.
        let status = unsafe {
            CMSampleBuffer::create_ready(None, Some(&block), Some(format), 1, 1, &timing, 1, &len, NonNull::from(&mut sample))
        };
        let sample = NonNull::new(sample).filter(|_| status == 0).ok_or_else(|| anyhow!("CMSampleBufferCreate failed ({status})"))?;
        // SAFETY: +1 reference from a Create function.
        let sample = unsafe { CFRetained::from_raw(sample) };

        // Synchronous decode: the callback runs before decode_frame returns,
        // in decode order. The PTS rides in the frame refcon.
        // SAFETY: valid session and sample.
        let status = unsafe { session.decode_frame(&sample, VTDecodeFrameFlags(0), pts as *mut c_void, null_mut()) };
        if status != 0 {
            bail!("VideoToolbox could not decode a frame ({status})");
        }
        let failed = self.output.failed.swap(0, Ordering::Relaxed);
        if failed != 0 {
            bail!("VideoToolbox could not decode a frame ({failed})");
        }
        Ok(self.collect())
    }

    fn flush(&mut self) -> Result<Vec<DecodedFrame>> {
        if let Some((_, session)) = &self.session {
            // SAFETY: valid session.
            unsafe {
                session.finish_delayed_frames();
                session.wait_for_asynchronous_frames();
            }
        }
        let mut out = self.collect();
        out.extend(self.reorder.drain());
        Ok(out)
    }

    fn reset(&mut self, discard_before: i64) -> Result<()> {
        self.close_session();
        self.output.frames.lock().unwrap().clear();
        self.reorder.drain();
        self.output.discard_before.store(discard_before, Ordering::Relaxed);
        self.seen_keyframe = false;
        if self.codec != kCMVideoCodecType_VP9 {
            self.open_session()?;
        }
        Ok(())
    }
}

impl Drop for VtDecoder {
    fn drop(&mut self) {
        self.close_session();
    }
}
