//! Video decoding. Each codec backend turns compressed access units into BGRA
//! frames tagged with the presentation timestamp they were fed.
//!
//! The OS hardware decoder is preferred where one exists (VideoToolbox on
//! macOS); otherwise H.264 uses OpenH264 and AV1 uses rav1d.

pub mod av1;
pub mod bitstream;
pub mod h264;
pub mod hevc;
#[cfg(target_os = "macos")]
pub mod videotoolbox;
pub mod yuv;

use anyhow::{Result, anyhow, bail};
use symphonia::core::codecs::video::well_known::extra_data::{
    VIDEO_EXTRA_DATA_ID_AV1_DECODER_CONFIG, VIDEO_EXTRA_DATA_ID_AVC_DECODER_CONFIG,
    VIDEO_EXTRA_DATA_ID_HEVC_DECODER_CONFIG,
};
use symphonia::core::codecs::video::well_known::{CODEC_ID_AV1, CODEC_ID_H264, CODEC_ID_HEVC, CODEC_ID_VP9};
use symphonia::core::codecs::video::{VideoCodecId, VideoCodecParameters};

pub struct DecodedFrame {
    /// Presentation timestamp in the track's timebase.
    pub pts: i64,
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

pub trait VideoDecoder: Send {
    /// Shown in the info panel, e.g. "VideoToolbox (hardware)".
    fn name(&self) -> &'static str;
    /// Decode one access unit. May return zero or more frames, in any order.
    fn decode(&mut self, data: &[u8], pts: i64) -> Result<Vec<DecodedFrame>>;
    /// Whether decoding can start at this access unit.
    fn is_keyframe(&self, data: &[u8]) -> bool;
    /// Whether no other frame refers to this access unit, so video that is
    /// behind can leave it undecoded without harming the frames after it.
    fn is_droppable(&self, _data: &[u8]) -> bool {
        false
    }
    /// Return frames still buffered inside the decoder (end of stream).
    fn flush(&mut self) -> Result<Vec<DecodedFrame>>;
    /// Discard all state after a seek. Frames until the next keyframe are
    /// dropped, and frames before `discard_before` are decoded (they are
    /// references) but not colour-converted or returned.
    fn reset(&mut self, discard_before: i64) -> Result<()>;
    /// The largest size frames can be shown at (physical pixels). Decoders
    /// that convert colour on the CPU may return smaller frames down to this
    /// size; others ignore it.
    fn set_output_limit(&mut self, _limit: Option<(u32, u32)>) {}
}

/// Puts frames from a decoder that emits decode order into display order.
/// `depth` is the most frames that can follow a frame in decode order but
/// precede it in display order, as the stream's SPS declares (0 for codecs
/// that never reorder at output, VP9 and AV1). Exact for conforming streams.
pub(crate) struct Reorder {
    depth: usize,
    held: Vec<DecodedFrame>,
}

impl Reorder {
    pub(crate) fn new(depth: usize) -> Self {
        Self { depth, held: Vec::new() }
    }

    pub(crate) fn push(&mut self, frames: impl IntoIterator<Item = DecodedFrame>) -> Vec<DecodedFrame> {
        for f in frames {
            let at = self.held.partition_point(|e| e.pts <= f.pts);
            self.held.insert(at, f);
        }
        let excess = self.held.len().saturating_sub(self.depth);
        self.held.drain(..excess).collect()
    }

    pub(crate) fn drain(&mut self) -> Vec<DecodedFrame> {
        std::mem::take(&mut self.held)
    }
}

pub fn codec_name(id: VideoCodecId) -> &'static str {
    match id {
        CODEC_ID_H264 => "H.264",
        CODEC_ID_HEVC => "HEVC",
        CODEC_ID_AV1 => "AV1",
        CODEC_ID_VP9 => "VP9",
        _ => "video",
    }
}

/// Set to force the software decoders (diagnostics, and their tests).
pub const FORCE_SOFTWARE_ENV: &str = "AIRSPLICE_PLAYER_SOFTWARE_VIDEO";

/// The best available decoder: hardware first, then software.
pub fn make_decoder(params: &VideoCodecParameters) -> Result<Box<dyn VideoDecoder>> {
    if std::env::var_os(FORCE_SOFTWARE_ENV).is_none() {
        match make_hardware_decoder(params) {
            Ok(Some(dec)) => return Ok(dec),
            Ok(None) => {}
            Err(e) => log::info!("hardware decoder unavailable for {}: {e:#}", codec_name(params.codec)),
        }
    }
    make_software_decoder(params).map_err(|e| match params.codec {
        CODEC_ID_HEVC | CODEC_ID_VP9 => anyhow!("{} video needs a hardware decoder, and none is available here", codec_name(params.codec)),
        _ => e,
    })
}

/// The platform's hardware decoder, or `None` on platforms without one.
#[allow(unused_variables)]
pub fn make_hardware_decoder(params: &VideoCodecParameters) -> Result<Option<Box<dyn VideoDecoder>>> {
    #[cfg(target_os = "macos")]
    return Ok(Some(Box::new(videotoolbox::VtDecoder::new(params)?)));
    #[cfg(target_os = "linux")]
    return match params.codec {
        // Stateless V4L2 HEVC decoders (Raspberry Pi 4 and 5).
        CODEC_ID_HEVC => {
            let hvcc = params
                .extra_data
                .iter()
                .find(|e| e.id == VIDEO_EXTRA_DATA_ID_HEVC_DECODER_CONFIG)
                .ok_or_else(|| anyhow!("HEVC track has no hvcC decoder configuration"))?;
            let nal_length_size = bitstream::hvcc_nal_length_size(&hvcc.data)?;
            let depth = bitstream::hevc_reorder_depth(&hvcc.data).unwrap_or(16);
            Ok(Some(Box::new(hevc::v4l2::V4l2HevcDecoder::new(&hvcc.data, nal_length_size, depth, keyframe_check(params)?)?)))
        }
        _ => Ok(None),
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    Ok(None)
}

pub fn make_software_decoder(params: &VideoCodecParameters) -> Result<Box<dyn VideoDecoder>> {
    let extra = |id| params.extra_data.iter().find(|e| e.id == id).map(|e| &*e.data);
    match params.codec {
        CODEC_ID_H264 => {
            let avcc = extra(VIDEO_EXTRA_DATA_ID_AVC_DECODER_CONFIG)
                .ok_or_else(|| anyhow!("H.264 track has no avcC decoder configuration"))?;
            Ok(Box::new(h264::H264Decoder::new(avcc)?))
        }
        CODEC_ID_AV1 => Ok(Box::new(av1::Av1Decoder::new(extra(VIDEO_EXTRA_DATA_ID_AV1_DECODER_CONFIG))?)),
        other => bail!("no software decoder for {} video", codec_name(other)),
    }
}

/// A standalone keyframe test for the demuxer, which needs it to land seeks
/// on decodable packets but does not own the decoder.
pub type KeyframeCheck = Box<dyn Fn(&[u8]) -> bool + Send>;

pub fn keyframe_check(params: &VideoCodecParameters) -> Result<KeyframeCheck> {
    let extra = |id| params.extra_data.iter().find(|e| e.id == id).map(|e| &*e.data);
    match params.codec {
        CODEC_ID_H264 => {
            let avcc = extra(VIDEO_EXTRA_DATA_ID_AVC_DECODER_CONFIG)
                .ok_or_else(|| anyhow!("H.264 track has no avcC decoder configuration"))?;
            let nal_length_size = h264::parse_avcc(avcc)?.nal_length_size;
            Ok(Box::new(move |data| h264::is_keyframe(data, nal_length_size)))
        }
        CODEC_ID_HEVC => {
            let hvcc = extra(VIDEO_EXTRA_DATA_ID_HEVC_DECODER_CONFIG)
                .ok_or_else(|| anyhow!("HEVC track has no hvcC decoder configuration"))?;
            let nal_length_size = bitstream::hvcc_nal_length_size(hvcc)?;
            Ok(Box::new(move |data| bitstream::hevc_is_keyframe(data, nal_length_size)))
        }
        CODEC_ID_AV1 => Ok(Box::new(av1::is_keyframe)),
        CODEC_ID_VP9 => Ok(Box::new(|data| bitstream::vp9_keyframe(data).is_some())),
        other => bail!("{} video is not supported", codec_name(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(pts: i64) -> DecodedFrame {
        DecodedFrame { pts, width: 1, height: 1, bgra: Vec::new() }
    }

    #[test]
    fn reorder_turns_decode_order_into_display_order() {
        // Pyramid GOP in decode order: I0 P4 B2 b1 b3 P8 B6 b5 b7 (depth 2).
        let mut r = Reorder::new(2);
        let mut out = Vec::new();
        for p in [0, 4, 2, 1, 3, 8, 6, 5, 7] {
            out.extend(r.push([frame(p)]).into_iter().map(|f| f.pts));
        }
        out.extend(r.drain().into_iter().map(|f| f.pts));
        assert_eq!(out, (0..=8).collect::<Vec<_>>());
    }

    #[test]
    fn depth_zero_passes_frames_straight_through() {
        let mut r = Reorder::new(0);
        assert_eq!(r.push([frame(5)]).len(), 1);
        assert!(r.drain().is_empty());
    }
}
