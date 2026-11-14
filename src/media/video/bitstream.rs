//! Container-independent bitstream facts for HEVC and VP9: keyframe
//! detection (seeking needs it whatever decoder runs) and the VP9
//! configuration record hardware decoders want.

use anyhow::{Result, bail};


/// `lengthSizeMinusOne + 1` from an `HEVCDecoderConfigurationRecord`.
pub fn hvcc_nal_length_size(hvcc: &[u8]) -> Result<usize> {
    match hvcc.get(21) {
        Some(b) if hvcc[0] == 1 => Ok((b & 0x03) as usize + 1),
        _ => bail!("invalid hvcC record"),
    }
}

/// An HEVC access unit decoding can start from: one containing an IRAP
/// picture (BLA, IDR or CRA; NAL types 16 to 21).
pub fn hevc_is_keyframe(data: &[u8], nal_length_size: usize) -> bool {
    let mut pos = 0;
    while pos + nal_length_size <= data.len() {
        let len = data[pos..pos + nal_length_size].iter().fold(0usize, |acc, b| (acc << 8) | *b as usize);
        pos += nal_length_size;
        let Some(nal) = data.get(pos..pos + len) else { return false };
        if let Some(&h) = nal.first()
            && (16..=21).contains(&((h >> 1) & 0x3f))
        {
            return true;
        }
        pos += len;
    }
    false
}

/// Minimal Exp-Golomb reader over a NAL payload, skipping emulation
/// prevention bytes.
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    byte: usize,
    bit: u8,
    zeros: u8,
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, byte: 0, bit: 0, zeros: 0 }
    }

    pub(crate) fn read_bit(&mut self) -> Option<u32> {
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

    pub(crate) fn read_bits(&mut self, n: u32) -> Option<u32> {
        (0..n).try_fold(0u32, |acc, _| Some((acc << 1) | self.read_bit()?))
    }

    pub(crate) fn read_se(&mut self) -> Option<i32> {
        let k = self.read_ue()?;
        Some(if k % 2 == 1 { k.div_ceil(2) as i32 } else { -((k / 2) as i32) })
    }

    pub(crate) fn read_ue(&mut self) -> Option<u32> {
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

/// The NAL units of one type in an `HEVCDecoderConfigurationRecord`.
fn hvcc_nal_units(hvcc: &[u8], nal_type: u8) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let Some(&arrays) = hvcc.get(22) else { return out };
    let mut pos = 23;
    for _ in 0..arrays {
        let Some(&kind) = hvcc.get(pos) else { break };
        let Some(count) = hvcc.get(pos + 1..pos + 3).map(|b| u16::from_be_bytes([b[0], b[1]])) else { break };
        pos += 3;
        for _ in 0..count {
            let Some(len) = hvcc.get(pos..pos + 2).map(|b| u16::from_be_bytes([b[0], b[1]]) as usize) else { return out };
            let Some(nal) = hvcc.get(pos + 2..pos + 2 + len) else { return out };
            if kind & 0x3f == nal_type {
                out.push(nal);
            }
            pos += 2 + len;
        }
    }
    out
}

/// `sps_max_num_reorder_pics` (highest sub-layer) from the record's SPS
/// (H.265 7.3.2.2): how many frames may come out of order.
pub fn hevc_reorder_depth(hvcc: &[u8]) -> Option<usize> {
    let sps = *hvcc_nal_units(hvcc, 33).first()?;
    let mut r = BitReader::new(sps.get(2..)?); // two-byte NAL header
    r.read_bits(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = r.read_bits(3)?;
    r.read_bit()?; // sps_temporal_id_nesting_flag
    // profile_tier_level(1, max_sub_layers_minus1)
    r.read_bits(32)?;
    r.read_bits(32)?;
    r.read_bits(24)?; // general profile (88 bits) + general_level_idc (8)
    r.read_bits(8)?;
    let mut present = Vec::new();
    for _ in 0..max_sub_layers_minus1 {
        present.push((r.read_bit()?, r.read_bit()?));
    }
    if max_sub_layers_minus1 > 0 {
        for _ in max_sub_layers_minus1..8 {
            r.read_bits(2)?;
        }
    }
    for (profile, level) in present {
        if profile == 1 {
            r.read_bits(32)?;
            r.read_bits(32)?;
            r.read_bits(24)?;
        }
        if level == 1 {
            r.read_bits(8)?;
        }
    }
    r.read_ue()?; // sps_seq_parameter_set_id
    if r.read_ue()? == 3 {
        r.read_bit()?; // separate_colour_plane_flag
    }
    r.read_ue()?; // pic_width_in_luma_samples
    r.read_ue()?; // pic_height_in_luma_samples
    if r.read_bit()? == 1 {
        for _ in 0..4 {
            r.read_ue()?; // conformance window
        }
    }
    r.read_ue()?; // bit_depth_luma_minus8
    r.read_ue()?; // bit_depth_chroma_minus8
    r.read_ue()?; // log2_max_pic_order_cnt_lsb_minus4
    let all_layers = r.read_bit()? == 1;
    let mut reorder = 0;
    for _ in if all_layers { 0 } else { max_sub_layers_minus1 }..=max_sub_layers_minus1 {
        r.read_ue()?; // sps_max_dec_pic_buffering_minus1
        reorder = r.read_ue()?;
        r.read_ue()?; // sps_max_latency_increase_plus1
    }
    Some(reorder as usize)
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> Option<u8> {
        let b = self.data.get(self.pos / 8)?;
        let v = (b >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(v)
    }
    fn bits(&mut self, n: u32) -> Option<u32> {
        (0..n).try_fold(0u32, |acc, _| Some((acc << 1) | self.bit()? as u32))
    }
}

/// What a VP9 keyframe header says about the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vp9Config {
    pub profile: u8,
    pub bit_depth: u8,
    /// `chromaSubsampling` as in `vpcC`: 1 = 4:2:0, 2 = 4:2:2, 3 = 4:4:4.
    pub chroma_subsampling: u8,
    pub full_range: bool,
}

/// Parse the uncompressed header at the start of a VP9 frame (or the first
/// frame of a superframe). `Some` only for a shown key frame.
pub fn vp9_keyframe(data: &[u8]) -> Option<Vp9Config> {
    let mut r = Bits { data, pos: 0 };
    if r.bits(2)? != 0b10 {
        return None; // frame_marker
    }
    let low = r.bit()?;
    let high = r.bit()?;
    let profile = (high << 1) | low;
    if profile == 3 {
        r.bit()?; // reserved_zero
    }
    if r.bit()? == 1 {
        return None; // show_existing_frame
    }
    if r.bit()? != 0 {
        return None; // frame_type: not KEY_FRAME
    }
    r.bits(2)?; // show_frame, error_resilient_mode
    if r.bits(24)? != 0x49_83_42 {
        return None; // frame_sync_code
    }
    let bit_depth = if profile >= 2 { if r.bit()? == 1 { 12 } else { 10 } } else { 8 };
    let color_space = r.bits(3)?;
    let (full_range, subsampling) = if color_space != 7 {
        let full_range = r.bit()? == 1;
        let ss = if profile == 1 || profile == 3 {
            let (x, y) = (r.bit()?, r.bit()?);
            match (x, y) {
                (1, 0) => 2,
                (0, 0) => 3,
                _ => 1,
            }
        } else {
            1
        };
        (full_range, ss)
    } else {
        (true, 3) // sRGB is always 4:4:4 full range
    };
    Some(Vp9Config { profile, bit_depth, chroma_subsampling: subsampling, full_range })
}

/// A `VPCodecConfigurationRecord` (`vpcC` box payload, version 1) with
/// BT.709 colour, for decoders that need one up front.
pub fn vpcc(c: &Vp9Config) -> Vec<u8> {
    vec![
        1, 0, 0, 0, // version 1, flags
        c.profile,
        0, // level: unspecified
        (c.bit_depth << 4) | (c.chroma_subsampling << 1) | u8::from(c.full_range),
        1, // colour primaries BT.709
        1, // transfer characteristics BT.709
        1, // matrix coefficients BT.709
        0, 0, // no codec initialisation data
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hevc_keyframes() {
        // NAL header type is bits 1..=6 of the first byte.
        let nal = |t: u8| [0, 0, 0, 2, t << 1, 1];
        assert!(hevc_is_keyframe(&nal(19), 4), "IDR_W_RADL");
        assert!(hevc_is_keyframe(&nal(21), 4), "CRA");
        assert!(!hevc_is_keyframe(&nal(1), 4), "TRAIL_R");
        assert!(!hevc_is_keyframe(&nal(35), 4), "AUD");
        let mut au = nal(35).to_vec();
        au.extend(nal(20));
        assert!(hevc_is_keyframe(&au, 4), "IDR after an AUD");
        assert!(!hevc_is_keyframe(&[0, 0, 0, 9, 40], 4), "truncated");
    }

    #[test]
    fn exp_golomb_skips_emulation_prevention() {
        // 0x00 0x00 0x03 0x01: the 0x03 is dropped, leaving 23 zero bits, a
        // 1, then 23 zero value bits: ue = 2^23 - 1.
        let mut r = BitReader::new(&[0x00, 0x00, 0x03, 0x01, 0x00, 0x00, 0x00]);
        assert_eq!(r.read_ue(), Some((1 << 23) - 1));
    }

    #[test]
    fn hevc_reorder_depth_from_a_real_record() {
        // ffprobe reports has_b_frames=2 for this x265 stream (bframes=3).
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bframes_hevc.mkv");
        let bytes = std::fs::read(path).unwrap();
        // Matroska CodecPrivate holds the hvcC record; find it by its
        // configurationVersion byte after the CodecPrivate element ID.
        let at = bytes.windows(2).position(|w| w == [0x63, 0xa2]).unwrap();
        let len = bytes[at + 2] & 0x7f; // one-byte EBML size
        let hvcc = &bytes[at + 3..at + 3 + len as usize];
        assert_eq!(hevc_reorder_depth(hvcc), Some(2));
        assert_eq!(hvcc_nal_length_size(hvcc).unwrap(), 4);
    }

    #[test]
    fn hvcc_length_size() {
        let mut hvcc = vec![0u8; 23];
        hvcc[0] = 1;
        hvcc[21] = 0xff;
        assert_eq!(hvcc_nal_length_size(&hvcc).unwrap(), 4);
        assert!(hvcc_nal_length_size(&hvcc[..10]).is_err());
    }

    #[test]
    fn vp9_headers() {
        // Profile 0 keyframe: marker 10, profile 00, show_existing 0,
        // frame_type 0, show_frame 1, error_res 0, sync 0x498342,
        // color_space 001 (BT.601), color_range 0.
        // Bits: 10 0 0 0 0 1 0 | 0100 1001 1000 0011 0100 0010 | 001 0
        let key = [0b1000_0010, 0x49, 0x83, 0x42, 0b0010_0000];
        assert_eq!(vp9_keyframe(&key), Some(Vp9Config { profile: 0, bit_depth: 8, chroma_subsampling: 1, full_range: false }));
        // Same with frame_type 1 (inter).
        assert_eq!(vp9_keyframe(&[0b1000_0110, 0x49, 0x83, 0x42, 0]), None);
        // show_existing_frame.
        assert_eq!(vp9_keyframe(&[0b1000_1000]), None);
        // Profile 2 (low 0, high 1), ten_or_twelve 0 -> 10 bit.
        // Bits: 10 0 1 0 0 1 0 | sync | 0 001 0 ...
        let p2 = [0b1001_0010, 0x49, 0x83, 0x42, 0b0001_0000];
        assert_eq!(vp9_keyframe(&p2).map(|c| (c.profile, c.bit_depth)), Some((2, 10)));
        assert_eq!(vpcc(&vp9_keyframe(&p2).unwrap())[6], (10 << 4) | (1 << 1));
    }
}
