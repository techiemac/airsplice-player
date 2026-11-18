//! H.264 through OpenH264. MP4 and Matroska store H.264 as length-prefixed
//! NAL units with the parameter sets in an `avcC` record; OpenH264 wants an
//! Annex B byte stream, so this module converts between the two.

use std::ptr::{from_mut, null_mut};

use anyhow::{Result, anyhow, bail};
use openh264::OpenH264API;
use openh264::decoder::{Decoder, DecoderConfig, Flush};
use openh264_sys2::{DECODER_OPTION, DECODER_OPTION_NUM_OF_FRAMES_REMAINING_IN_BUFFER, SBufferInfo, dsErrorFree};

use super::yuv::{self, Planes, Subsampling};
use super::bitstream::BitReader;
use super::{DecodedFrame, Reorder, VideoDecoder};

const START_CODE: [u8; 4] = [0, 0, 0, 1];
const NAL_SLICE: u8 = 1;
const NAL_IDR: u8 = 5;
/// OpenH264 rejects larger access units outright (`MAX_ACCESS_UNIT_CAPACITY`,
/// the level 5.2 bound); streams beyond their level's bitrate hit it.
const MAX_ACCESS_UNIT: usize = 7_077_888;

/// Parameter sets and NAL length size from an `AVCDecoderConfigurationRecord`.
#[derive(Debug, PartialEq)]
pub struct AvcConfig {
    pub nal_length_size: usize,
    /// SPS and PPS NAL units already prefixed with start codes.
    pub annexb_headers: Vec<u8>,
    /// The first SPS NAL unit (header byte included).
    pub sps: Vec<u8>,
}

pub fn parse_avcc(data: &[u8]) -> Result<AvcConfig> {
    if data.len() < 7 || data[0] != 1 {
        bail!("invalid avcC record");
    }
    let truncated = || anyhow!("truncated avcC");
    let nal_length_size = (data[4] & 0x03) as usize + 1;
    let mut headers = Vec::new();
    let mut sps = Vec::new();
    let mut pos = 5;
    for count_mask in [0x1f, 0xff] {
        let count = (*data.get(pos).ok_or_else(truncated)? & count_mask) as usize;
        pos += 1;
        for _ in 0..count {
            let len = u16::from_be_bytes(data.get(pos..pos + 2).ok_or_else(truncated)?.try_into()?) as usize;
            pos += 2;
            let nal = data.get(pos..pos + len).ok_or_else(truncated)?;
            if sps.is_empty() && count_mask == 0x1f {
                sps = nal.to_vec();
            }
            headers.extend_from_slice(&START_CODE);
            headers.extend_from_slice(nal);
            pos += len;
        }
    }
    Ok(AvcConfig { nal_length_size, annexb_headers: headers, sps })
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

/// How many frames may precede a frame in decode order but follow it in
/// display order (H.264 7.3.2.1 and E.1.1): the VUI's
/// `max_num_reorder_frames` when present, else a safe bound (0 for streams
/// that cannot reorder, otherwise the level's DPB size from Table A-1).
pub fn reorder_depth(sps: &[u8]) -> Option<usize> {
    let mut r = BitReader::new(sps.get(1..)?);
    let profile = r.read_bits(8)?;
    r.read_bits(8)?; // constraint flags
    let level = r.read_bits(8)?;
    r.read_ue()?; // seq_parameter_set_id
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        let chroma_format = r.read_ue()?;
        if chroma_format == 3 {
            r.read_bit()?;
        }
        r.read_ue()?; // bit_depth_luma_minus8
        r.read_ue()?; // bit_depth_chroma_minus8
        r.read_bit()?; // qpprime_y_zero_transform_bypass_flag
        if r.read_bit()? == 1 {
            for i in 0..if chroma_format == 3 { 12 } else { 8 } {
                if r.read_bit()? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i32, 8i32);
                    for _ in 0..size {
                        if next != 0 {
                            next = (last + r.read_se()? + 256) % 256;
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    r.read_ue()?; // log2_max_frame_num_minus4
    let poc_type = r.read_ue()?;
    match poc_type {
        0 => {
            r.read_ue()?;
        }
        1 => {
            r.read_bit()?;
            r.read_se()?;
            r.read_se()?;
            for _ in 0..r.read_ue()? {
                r.read_se()?;
            }
        }
        _ => {}
    }
    r.read_ue()?; // max_num_ref_frames
    r.read_bit()?; // gaps_in_frame_num_value_allowed_flag
    let width_mbs = r.read_ue()? as usize + 1;
    let height_units = r.read_ue()? as usize + 1;
    let frame_mbs_only = r.read_bit()?;
    if frame_mbs_only == 0 {
        r.read_bit()?;
    }
    r.read_bit()?; // direct_8x8_inference_flag
    if r.read_bit()? == 1 {
        for _ in 0..4 {
            r.read_ue()?;
        }
    }
    // Baseline and POC type 2 streams output in decode order.
    let cannot_reorder = profile == 66 || poc_type == 2;
    let fallback = || {
        if cannot_reorder {
            return 0;
        }
        let frame_mbs = width_mbs * height_units * (2 - frame_mbs_only as usize);
        let max_dpb_mbs = match level {
            // Level 1b signalled as idc 11 gets level 1.1's (larger) bound.
            9 | 10 => 396,
            11 => 900,
            12 | 13 | 20 => 2376,
            21 => 4752,
            22 | 30 => 8100,
            31 => 18000,
            32 => 20480,
            40 | 41 => 32768,
            42 => 34816,
            50 => 110400,
            51 | 52 => 184320,
            _ => 696320,
        };
        (max_dpb_mbs / frame_mbs.max(1)).min(16)
    };
    if r.read_bit()? == 0 {
        return Some(fallback()); // no VUI
    }
    if r.read_bit()? == 1 && r.read_bits(8)? == 255 {
        r.read_bits(32)?; // sar_width, sar_height
    }
    if r.read_bit()? == 1 {
        r.read_bit()?; // overscan_appropriate_flag
    }
    if r.read_bit()? == 1 {
        r.read_bits(4)?; // video_format, video_full_range_flag
        if r.read_bit()? == 1 {
            r.read_bits(24)?; // colour description
        }
    }
    if r.read_bit()? == 1 {
        r.read_ue()?;
        r.read_ue()?;
    }
    if r.read_bit()? == 1 {
        r.read_bits(32)?;
        r.read_bits(32)?;
        r.read_bit()?;
    }
    let hrd = |r: &mut BitReader<'_>| -> Option<()> {
        let count = r.read_ue()? + 1;
        r.read_bits(8)?;
        for _ in 0..count {
            r.read_ue()?;
            r.read_ue()?;
            r.read_bit()?;
        }
        r.read_bits(20)?;
        Some(())
    };
    let nal_hrd = r.read_bit()? == 1;
    if nal_hrd {
        hrd(&mut r)?;
    }
    let vcl_hrd = r.read_bit()? == 1;
    if vcl_hrd {
        hrd(&mut r)?;
    }
    if nal_hrd || vcl_hrd {
        r.read_bit()?;
    }
    r.read_bit()?; // pic_struct_present_flag
    if r.read_bit()? == 0 {
        return Some(fallback()); // no bitstream_restriction
    }
    r.read_bit()?;
    for _ in 0..4 {
        r.read_ue()?;
    }
    Some(r.read_ue()? as usize)
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

/// Whether every slice of the access unit has `nal_ref_idc` 0: no other
/// picture refers to it, so it can be left undecoded.
pub fn is_droppable(data: &[u8], nal_length_size: usize) -> bool {
    let mut slices = 0;
    for nal in nal_units(data, nal_length_size) {
        let Some(&header) = nal.first() else { continue };
        // Slices, data partition A (which carries the slice header), IDR.
        if matches!(header & 0x1f, NAL_SLICE | 2 | NAL_IDR) {
            if header & 0x60 != 0 {
                return false;
            }
            slices += 1;
        }
    }
    slices > 0
}

pub struct H264Decoder {
    decoder: Decoder,
    config: AvcConfig,
    scratch: Vec<u8>,
    /// Frames before the first keyframe after a reset reference pictures the
    /// decoder never saw; they are skipped rather than shown as garbage.
    seen_keyframe: bool,
    discard_before: i64,
    /// OpenH264's low-latency API emits decode order.
    reorder: Reorder,
}

impl H264Decoder {
    pub fn new(avcc: &[u8]) -> Result<Self> {
        let config = parse_avcc(avcc)?;
        // An unparseable SPS gets the largest bound: correct, if slower to
        // show the first frame.
        let reorder_depth = reorder_depth(&config.sps).unwrap_or(16);
        Ok(Self {
            decoder: Self::make_decoder()?,
            config,
            scratch: Vec::new(),
            seen_keyframe: false,
            discard_before: i64::MIN,
            reorder: Reorder::new(reorder_depth),
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
    fn name(&self) -> &'static str {
        "OpenH264 (software)"
    }

    fn is_keyframe(&self, data: &[u8]) -> bool {
        is_keyframe(data, self.config.nal_length_size)
    }

    fn is_droppable(&self, data: &[u8]) -> bool {
        is_droppable(data, self.config.nal_length_size)
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
        if annexb.len() > MAX_ACCESS_UNIT {
            log::warn!("OpenH264: frame of {} bytes is over its {MAX_ACCESS_UNIT}-byte limit", annexb.len());
            self.scratch = annexb;
            bail!("bitrate too high for software decoding");
        }

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
        let frame = self.frame_from(&info, &dst);
        self.scratch = annexb;
        if state != dsErrorFree {
            // Concealable errors still often produce a picture; keep going
            // like other players do. No picture is a failure to report.
            log::debug!("openh264 decode state {state:#x}");
            if frame.is_none() {
                bail!("damaged or unsupported data");
            }
        }
        Ok(self.reorder.push(frame))
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
        let mut out = self.reorder.push(frames);
        out.extend(self.reorder.drain());
        Ok(out)
    }

    fn reset(&mut self, discard_before: i64) -> Result<()> {
        self.decoder = Self::make_decoder()?;
        self.seen_keyframe = false;
        self.discard_before = discard_before;
        self.reorder.drain();
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
    fn a_frame_over_openh264s_limit_is_an_error_not_a_blank_picture() {
        let avcc = [1, 0x64, 0, 0x1f, 0xff, 0xe1, 0, 3, 0x67, 0xaa, 0xbb, 1, 0, 2, 0x68, 0xcc];
        let mut dec = H264Decoder::new(&avcc).unwrap();
        // One IDR NAL unit just over the limit.
        let len = MAX_ACCESS_UNIT;
        let mut packet = (len as u32).to_be_bytes().to_vec();
        packet.push(0x65);
        packet.resize(4 + len, 0);
        let Err(err) = dec.decode(&packet, 0) else { panic!("decoded a frame over the limit") };
        let err = err.to_string();
        assert_eq!(err, "bitrate too high for software decoding");
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

}
