//! H.264 through OpenH264. MP4 and Matroska store H.264 as length-prefixed
//! NAL units with the parameter sets in an `avcC` record; OpenH264 wants an
//! Annex B byte stream, so this module converts between the two.

use std::ptr::{from_mut, null_mut};

use anyhow::{Result, anyhow, bail};
use openh264::OpenH264API;
use openh264::decoder::{Decoder, DecoderConfig, Flush};
use openh264_sys2::{DECODER_OPTION, DECODER_OPTION_NUM_OF_FRAMES_REMAINING_IN_BUFFER, SBufferInfo, dsErrorFree};

use super::yuv::{self, Planes, Subsampling};
use super::{DecodedFrame, VideoDecoder};

const START_CODE: [u8; 4] = [0, 0, 0, 1];
const NAL_SLICE: u8 = 1;
const NAL_IDR: u8 = 5;

/// Parameter sets and NAL length size from an `AVCDecoderConfigurationRecord`.
#[derive(Debug, PartialEq)]
pub struct AvcConfig {
    pub nal_length_size: usize,
    /// SPS and PPS NAL units already prefixed with start codes.
    pub annexb_headers: Vec<u8>,
}

pub fn parse_avcc(data: &[u8]) -> Result<AvcConfig> {
    if data.len() < 7 || data[0] != 1 {
        bail!("invalid avcC record");
    }
    let truncated = || anyhow!("truncated avcC");
    let nal_length_size = (data[4] & 0x03) as usize + 1;
    let mut headers = Vec::new();
    let mut pos = 5;
    for count_mask in [0x1f, 0xff] {
        let count = (*data.get(pos).ok_or_else(truncated)? & count_mask) as usize;
        pos += 1;
        for _ in 0..count {
            let len = u16::from_be_bytes(data.get(pos..pos + 2).ok_or_else(truncated)?.try_into()?) as usize;
            pos += 2;
            headers.extend_from_slice(&START_CODE);
            headers.extend_from_slice(data.get(pos..pos + len).ok_or_else(truncated)?);
            pos += len;
        }
    }
    Ok(AvcConfig { nal_length_size, annexb_headers: headers })
}

/// Iterate the NAL units of a length-prefixed access unit.
fn nal_units(data: &[u8], nal_length_size: usize) -> impl Iterator<Item = &[u8]> {
    let mut pos = 0;
    std::iter::from_fn(move || {
        if pos + nal_length_size > data.len() {
            return None;
        }
        let len = data[pos..pos + nal_length_size].iter().fold(0usize, |acc, b| (acc << 8) | *b as usize);
        pos += nal_length_size;
        let nal = data.get(pos..pos + len)?;
        pos += len;
        Some(nal)
    })
}

/// Rewrite length-prefixed NAL units as Annex B.
pub fn to_annexb(data: &[u8], nal_length_size: usize, out: &mut Vec<u8>) {
    for nal in nal_units(data, nal_length_size) {
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(nal);
    }
}

/// Minimal Exp-Golomb reader over a NAL payload, skipping emulation
/// prevention bytes.
struct BitReader<'a> {
    data: &'a [u8],
    byte: usize,
    bit: u8,
    zeros: u8,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, byte: 0, bit: 0, zeros: 0 }
    }

    fn read_bit(&mut self) -> Option<u32> {
        if self.bit == 0 {
            // 0x00 0x00 0x03 -> drop the 0x03.
            if self.zeros >= 2 && self.data.get(self.byte) == Some(&3) {
                self.byte += 1;
                self.zeros = 0;
            }
            let b = *self.data.get(self.byte)?;
            self.zeros = if b == 0 { self.zeros + 1 } else { 0 };
        }
        let b = self.data[self.byte];
        let v = (b >> (7 - self.bit)) & 1;
        self.bit += 1;
        if self.bit == 8 {
            self.bit = 0;
            self.byte += 1;
        }
        Some(v as u32)
    }

    fn read_ue(&mut self) -> Option<u32> {
        let mut leading = 0;
        while self.read_bit()? == 0 {
            leading += 1;
            if leading > 31 {
                return None;
            }
        }
        let mut v = 0u32;
        for _ in 0..leading {
            v = (v << 1) | self.read_bit()?;
        }
        Some((1u32 << leading) - 1 + v)
    }
}

/// An access unit decoding can start from: an IDR, or an intra-coded first
/// slice (streams with open GOPs and recovery points have few IDRs).
pub fn is_keyframe(data: &[u8], nal_length_size: usize) -> bool {
    nal_units(data, nal_length_size).any(|nal| {
        let Some(&header) = nal.first() else { return false };
        match header & 0x1f {
            NAL_IDR => true,
            NAL_SLICE => {
                let mut r = BitReader::new(&nal[1..]);
                let first_mb = r.read_ue();
                let slice_type = r.read_ue();
                // slice_type 2/7 = I, 4/9 = SI.
                first_mb == Some(0) && matches!(slice_type.map(|t| t % 5), Some(2 | 4))
            }
            _ => false,
        }
    })
}

pub struct H264Decoder {
    decoder: Decoder,
    config: AvcConfig,
    scratch: Vec<u8>,
    /// Frames before the first keyframe after a reset reference pictures the
    /// decoder never saw; they are skipped rather than shown as garbage.
    seen_keyframe: bool,
    discard_before: i64,
}

impl H264Decoder {
    pub fn new(avcc: &[u8]) -> Result<Self> {
        let config = parse_avcc(avcc)?;
        Ok(Self {
            decoder: Self::make_decoder()?,
            config,
            scratch: Vec::new(),
            seen_keyframe: false,
            discard_before: i64::MIN,
        })
    }

    fn make_decoder() -> Result<Decoder> {
        let cfg = DecoderConfig::new().flush_after_decode(Flush::NoFlush);
        Decoder::with_api_config(OpenH264API::from_source(), cfg).map_err(|e| anyhow!("openh264: {e}"))
    }

    fn frame_from(&self, info: &SBufferInfo, dst: &[*mut u8; 3]) -> Option<DecodedFrame> {
        if info.iBufferStatus != 1 || dst.iter().any(|p| p.is_null()) {
            return None;
        }
        let pts = info.uiOutYuvTimeStamp as i64;
        if pts < self.discard_before {
            return None;
        }
        // SAFETY: iBufferStatus == 1 means the decoder filled sSystemBuffer.
        let sys = unsafe { info.UsrData.sSystemBuffer };
        let (w, h) = (sys.iWidth as usize, sys.iHeight as usize);
        let (ys, cs) = (sys.iStride[0] as usize, sys.iStride[1] as usize);
        let ch = h.div_ceil(2);
        // SAFETY: OpenH264 output is I420 with the strides it reports; the
        // planes stay valid until the next decode call, and we copy out now.
        let (y, u, v) = unsafe {
            (
                std::slice::from_raw_parts(dst[0], ys * h),
                std::slice::from_raw_parts(dst[1], cs * ch),
                std::slice::from_raw_parts(dst[2], cs * ch),
            )
        };
        let bgra = yuv::to_bgra(&Planes {
            y,
            u,
            v,
            y_stride: ys,
            uv_stride: cs,
            width: w,
            height: h,
            subsampling: Subsampling::S420,
            bit_depth: 8,
        });
        Some(DecodedFrame { pts, width: w as u32, height: h as u32, bgra })
    }
}

impl VideoDecoder for H264Decoder {
    fn is_keyframe(&self, data: &[u8]) -> bool {
        is_keyframe(data, self.config.nal_length_size)
    }

    fn decode(&mut self, data: &[u8], pts: i64) -> Result<Vec<DecodedFrame>> {
        if !self.seen_keyframe {
            if !self.is_keyframe(data) {
                return Ok(Vec::new());
            }
            self.seen_keyframe = true;
        }
        let mut annexb = std::mem::take(&mut self.scratch);
        annexb.clear();
        // Repeat the parameter sets ahead of every keyframe: cheap, and makes
        // seeking robust.
        if self.is_keyframe(data) {
            annexb.extend_from_slice(&self.config.annexb_headers);
        }
        to_annexb(data, self.config.nal_length_size, &mut annexb);

        let mut dst = [null_mut::<u8>(); 3];
        let mut info = SBufferInfo { uiInBsTimeStamp: pts as u64, ..Default::default() };
        // SAFETY: valid decoder, the input slice outlives the call, and the
        // out-params are properly sized.
        let state = unsafe {
            self.decoder.raw_api().decode_frame_no_delay(
                annexb.as_ptr(),
                annexb.len() as i32,
                from_mut(&mut dst).cast(),
                &raw mut info,
            )
        };
        if state != dsErrorFree {
            // Concealable errors still often produce a picture; keep going
            // like other players do.
            log::debug!("openh264 decode state {state:#x}");
        }
        let frame = self.frame_from(&info, &dst);
        self.scratch = annexb;
        Ok(frame.into_iter().collect())
    }

    fn flush(&mut self) -> Result<Vec<DecodedFrame>> {
        let mut frames = Vec::new();
        let mut remaining: DECODER_OPTION = 0;
        // SAFETY: valid decoder; get_option writes one DECODER_OPTION.
        unsafe {
            self.decoder
                .raw_api()
                .get_option(DECODER_OPTION_NUM_OF_FRAMES_REMAINING_IN_BUFFER, (&raw mut remaining).cast());
        }
        for _ in 0..remaining {
            let mut dst = [null_mut::<u8>(); 3];
            let mut info = SBufferInfo::default();
            // SAFETY: as in decode.
            unsafe {
                self.decoder.raw_api().flush_frame(from_mut(&mut dst).cast(), &raw mut info);
            }
            frames.extend(self.frame_from(&info, &dst));
        }
        Ok(frames)
    }

    fn reset(&mut self, discard_before: i64) -> Result<()> {
        self.decoder = Self::make_decoder()?;
        self.seen_keyframe = false;
        self.discard_before = discard_before;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avcc_and_annexb_conversion() {
        // version, profile, compat, level, 0xFF (4-byte lengths), 1 SPS, 1 PPS.
        let avcc = [1, 0x64, 0, 0x1f, 0xff, 0xe1, 0, 3, 0x67, 0xaa, 0xbb, 1, 0, 2, 0x68, 0xcc];
        let cfg = parse_avcc(&avcc).unwrap();
        assert_eq!(cfg.nal_length_size, 4);
        assert_eq!(cfg.annexb_headers, vec![0, 0, 0, 1, 0x67, 0xaa, 0xbb, 0, 0, 0, 1, 0x68, 0xcc]);

        let packet = [0, 0, 0, 2, 0x65, 0x11, 0, 0, 0, 1, 0x06];
        let mut out = Vec::new();
        to_annexb(&packet, 4, &mut out);
        assert_eq!(out, vec![0, 0, 0, 1, 0x65, 0x11, 0, 0, 0, 1, 0x06]);
    }

    #[test]
    fn truncated_avcc_is_rejected() {
        assert!(parse_avcc(&[1, 0x64, 0, 0x1f, 0xff, 0xe1, 0, 9, 0x67]).is_err());
    }

    #[test]
    fn keyframe_detection() {
        // IDR NAL.
        assert!(is_keyframe(&[0, 0, 0, 2, 0x65, 0x88], 4));
        // Non-IDR slice, first_mb=0 (ue "1"), slice_type=7 I (ue "0001000"):
        // bits 1 0001000 -> 0x88.
        assert!(is_keyframe(&[0, 0, 0, 2, 0x41, 0x88], 4));
        // Non-IDR slice, slice_type=5 P (ue "00110"): bits 1 00110 -> 0x98.
        assert!(!is_keyframe(&[0, 0, 0, 2, 0x41, 0x98], 4));
        // SEI only.
        assert!(!is_keyframe(&[0, 0, 0, 1, 0x06], 4));
        // Truncated length prefix.
        assert!(!is_keyframe(&[0, 0, 0, 9, 0x65], 4));
    }

    #[test]
    fn exp_golomb_skips_emulation_prevention() {
        // 0x00 0x00 0x03 0x01: the 0x03 is dropped, leaving 23 zero bits, a
        // 1, then 23 zero value bits: ue = 2^23 - 1.
        let mut r = BitReader::new(&[0x00, 0x00, 0x03, 0x01, 0x00, 0x00, 0x00]);
        assert_eq!(r.read_ue(), Some((1 << 23) - 1));
    }
}
