//! Video decoding. Each codec backend turns compressed access units into BGRA
//! frames tagged with the presentation timestamp they were fed.

pub mod av1;
pub mod h264;
pub mod yuv;

use anyhow::{Result, anyhow, bail};
use symphonia::core::codecs::video::well_known::extra_data::{
    VIDEO_EXTRA_DATA_ID_AV1_DECODER_CONFIG, VIDEO_EXTRA_DATA_ID_AVC_DECODER_CONFIG,
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
    /// Decode one access unit. May return zero or more frames, in any order.
    fn decode(&mut self, data: &[u8], pts: i64) -> Result<Vec<DecodedFrame>>;
    /// Whether decoding can start at this access unit.
    fn is_keyframe(&self, data: &[u8]) -> bool;
    /// Return frames still buffered inside the decoder (end of stream).
    fn flush(&mut self) -> Result<Vec<DecodedFrame>>;
    /// Discard all state after a seek. Frames until the next keyframe are
    /// dropped, and frames before `discard_before` are decoded (they are
    /// references) but not colour-converted or returned.
    fn reset(&mut self, discard_before: i64) -> Result<()>;
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

pub fn make_decoder(params: &VideoCodecParameters) -> Result<Box<dyn VideoDecoder>> {
    let extra = |id| params.extra_data.iter().find(|e| e.id == id).map(|e| &*e.data);
    match params.codec {
        CODEC_ID_H264 => {
            let avcc = extra(VIDEO_EXTRA_DATA_ID_AVC_DECODER_CONFIG)
                .ok_or_else(|| anyhow!("H.264 track has no avcC decoder configuration"))?;
            Ok(Box::new(h264::H264Decoder::new(avcc)?))
        }
        CODEC_ID_AV1 => Ok(Box::new(av1::Av1Decoder::new(extra(VIDEO_EXTRA_DATA_ID_AV1_DECODER_CONFIG))?)),
        other => bail!("{} video is not supported yet (H.264 and AV1 are)", codec_name(other)),
    }
}

/// A standalone keyframe test for the demuxer, which needs it to land seeks
/// on decodable packets but does not own the decoder.
pub type KeyframeCheck = Box<dyn Fn(&[u8]) -> bool + Send>;

pub fn keyframe_check(params: &VideoCodecParameters) -> Result<KeyframeCheck> {
    match params.codec {
        CODEC_ID_H264 => {
            let avcc = params
                .extra_data
                .iter()
                .find(|e| e.id == VIDEO_EXTRA_DATA_ID_AVC_DECODER_CONFIG)
                .ok_or_else(|| anyhow!("H.264 track has no avcC decoder configuration"))?;
            let nal_length_size = h264::parse_avcc(&avcc.data)?.nal_length_size;
            Ok(Box::new(move |data| h264::is_keyframe(data, nal_length_size)))
        }
        CODEC_ID_AV1 => Ok(Box::new(av1::is_keyframe)),
        other => bail!("{} video is not supported yet (H.264 and AV1 are)", codec_name(other)),
    }
}
