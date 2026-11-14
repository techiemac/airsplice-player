//! Turns HEVC access units into the controls a stateless V4L2 decoder needs.
//!
//! Parsing (VPS/SPS/PPS/slice headers) is cros-codecs'; this module derives
//! what the hardware cannot: picture order counts (H.265 8.3.1), reference
//! picture sets (8.3.2), reference picture lists (8.3.4), and which earlier
//! pictures (identified by a tag, the timestamp of the buffer they were
//! decoded from) must stay available as references.
//!
//! Display order is not handled here: frames are converted as soon as they
//! are decoded and reordered by PTS downstream, so the DPB only tracks
//! references.

use std::io::Cursor;
use std::rc::Rc;

use anyhow::{Result, anyhow, bail};
use cros_codecs::codec::h265::parser::{
    Nalu, NaluType, Parser, Pps, PredWeightTable, ScalingLists, ShortTermRefPicSet, Slice, SliceHeader, Sps,
};

use super::uapi;

/// A decoded picture still in the DPB.
#[derive(Clone, Copy, Debug, PartialEq)]
struct RefPic {
    poc: i32,
    tag: u64,
    long_term: bool,
}

/// Everything needed to decode one picture.
pub struct FrameControls {
    pub sps: uapi::Sps,
    pub pps: uapi::Pps,
    pub scaling: Option<uapi::ScalingMatrix>,
    pub slices: Vec<uapi::SliceParams>,
    pub decode: uapi::DecodeParams,
    /// The picture's slice NAL units back to back, without start codes.
    pub data: Vec<u8>,
    /// Identifies this picture when later pictures reference it.
    pub tag: u64,
    /// Tags of earlier pictures that must stay available after this one is
    /// decoded (the DPB). Buffers holding any other tag can be reused.
    pub live_tags: Vec<u64>,
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    /// Reference pictures the stream may keep (sps_max_dec_pic_buffering).
    pub max_dpb: usize,
}

pub struct HevcFrames {
    parser: Parser,
    nal_length_size: usize,
    dpb: Vec<RefPic>,
    prev_tid0_poc: i32,
    /// The next IRAP starts a new coded video sequence (NoRaslOutputFlag).
    new_sequence: bool,
    waiting_for_irap: bool,
    /// RASL pictures after a CRA/BLA that started a sequence reference
    /// pictures the decoder never saw; they are skipped.
    skip_rasl: bool,
    next_tag: u64,
}

/// NAL units from an `HEVCDecoderConfigurationRecord`, in order.
fn hvcc_nal_units(hvcc: &[u8]) -> Result<Vec<&[u8]>> {
    let truncated = || anyhow!("truncated hvcC record");
    let mut out = Vec::new();
    let arrays = *hvcc.get(22).ok_or_else(truncated)?;
    let mut pos = 23;
    for _ in 0..arrays {
        let count = u16::from_be_bytes(hvcc.get(pos + 1..pos + 3).ok_or_else(truncated)?.try_into()?) as usize;
        pos += 3;
        for _ in 0..count {
            let len = u16::from_be_bytes(hvcc.get(pos..pos + 2).ok_or_else(truncated)?.try_into()?) as usize;
            out.push(hvcc.get(pos + 2..pos + 2 + len).ok_or_else(truncated)?);
            pos += 2 + len;
        }
    }
    Ok(out)
}

fn annex_b<'a>(nals: impl IntoIterator<Item = &'a [u8]>) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nals {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
    }
    out
}

fn length_prefixed(data: &[u8], len_size: usize) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + len_size <= data.len() {
        let len = data[pos..pos + len_size].iter().fold(0usize, |a, b| (a << 8) | *b as usize);
        pos += len_size;
        let Some(nal) = data.get(pos..pos + len) else { break };
        out.push(nal);
        pos += len;
    }
    out
}

/// Sub-layer non-reference pictures (TRAIL_N, TSA_N, STSA_N, RADL_N,
/// RASL_N and the reserved even types up to 14).
fn is_sub_layer_non_reference(t: NaluType) -> bool {
    let t = t as u32;
    t <= 14 && t.is_multiple_of(2)
}

/// H.265 6.5.3: up-right diagonal scan order for a `size`x`size` block.
fn up_right_diagonal(size: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::with_capacity(size * size);
    let (mut x, mut y): (isize, isize) = (0, 0);
    while out.len() < size * size {
        while y >= 0 {
            if (x as usize) < size && (y as usize) < size {
                out.push((x as usize, y as usize));
            }
            y -= 1;
            x += 1;
        }
        y = x;
        x = 0;
    }
    out
}

fn to_raster<const N: usize>(coded: &[u8; N], size: usize) -> [u8; N] {
    let mut raster = [0u8; N];
    for (i, (x, y)) in up_right_diagonal(size).into_iter().enumerate() {
        raster[y * size + x] = coded[i];
    }
    raster
}

fn scaling_matrix(lists: &ScalingLists) -> uapi::ScalingMatrix {
    let mut m = uapi::ScalingMatrix::default();
    for i in 0..6 {
        m.scaling_list_4x4[i] = to_raster(&lists.scaling_list_4x4[i], 4);
        m.scaling_list_8x8[i] = to_raster(&lists.scaling_list_8x8[i], 8);
        m.scaling_list_16x16[i] = to_raster(&lists.scaling_list_16x16[i], 8);
        m.scaling_list_dc_coef_16x16[i] = (lists.scaling_list_dc_coef_minus8_16x16[i] + 8) as u8;
    }
    // Version 1 streams code 32x32 lists for matrixId 0 and 3 only.
    for (k, i) in [0, 3].into_iter().enumerate() {
        m.scaling_list_32x32[k] = to_raster(&lists.scaling_list_32x32[i], 8);
        m.scaling_list_dc_coef_32x32[k] = (lists.scaling_list_dc_coef_minus8_32x32[i] + 8) as u8;
    }
    m
}

fn uapi_sps(sps: &Sps) -> uapi::Sps {
    let top = sps.max_sub_layers_minus1 as usize;
    let mut flags = 0;
    for (on, flag) in [
        (sps.separate_colour_plane_flag, uapi::SPS_FLAG_SEPARATE_COLOUR_PLANE),
        (sps.scaling_list_enabled_flag, uapi::SPS_FLAG_SCALING_LIST_ENABLED),
        (sps.amp_enabled_flag, uapi::SPS_FLAG_AMP_ENABLED),
        (sps.sample_adaptive_offset_enabled_flag, uapi::SPS_FLAG_SAMPLE_ADAPTIVE_OFFSET),
        (sps.pcm_enabled_flag, uapi::SPS_FLAG_PCM_ENABLED),
        (sps.pcm_loop_filter_disabled_flag, uapi::SPS_FLAG_PCM_LOOP_FILTER_DISABLED),
        (sps.long_term_ref_pics_present_flag, uapi::SPS_FLAG_LONG_TERM_REF_PICS_PRESENT),
        (sps.temporal_mvp_enabled_flag, uapi::SPS_FLAG_SPS_TEMPORAL_MVP_ENABLED),
        (sps.strong_intra_smoothing_enabled_flag, uapi::SPS_FLAG_STRONG_INTRA_SMOOTHING_ENABLED),
    ] {
        if on {
            flags |= flag;
        }
    }
    uapi::Sps {
        video_parameter_set_id: sps.video_parameter_set_id,
        seq_parameter_set_id: sps.seq_parameter_set_id,
        pic_width_in_luma_samples: sps.pic_width_in_luma_samples,
        pic_height_in_luma_samples: sps.pic_height_in_luma_samples,
        bit_depth_luma_minus8: sps.bit_depth_luma_minus8,
        bit_depth_chroma_minus8: sps.bit_depth_chroma_minus8,
        log2_max_pic_order_cnt_lsb_minus4: sps.log2_max_pic_order_cnt_lsb_minus4,
        sps_max_dec_pic_buffering_minus1: sps.max_dec_pic_buffering_minus1[top],
        sps_max_num_reorder_pics: sps.max_num_reorder_pics[top],
        sps_max_latency_increase_plus1: sps.max_latency_increase_plus1[top],
        log2_min_luma_coding_block_size_minus3: sps.log2_min_luma_coding_block_size_minus3,
        log2_diff_max_min_luma_coding_block_size: sps.log2_diff_max_min_luma_coding_block_size,
        log2_min_luma_transform_block_size_minus2: sps.log2_min_luma_transform_block_size_minus2,
        log2_diff_max_min_luma_transform_block_size: sps.log2_diff_max_min_luma_transform_block_size,
        max_transform_hierarchy_depth_inter: sps.max_transform_hierarchy_depth_inter,
        max_transform_hierarchy_depth_intra: sps.max_transform_hierarchy_depth_intra,
        pcm_sample_bit_depth_luma_minus1: sps.pcm_sample_bit_depth_luma_minus1,
        pcm_sample_bit_depth_chroma_minus1: sps.pcm_sample_bit_depth_chroma_minus1,
        log2_min_pcm_luma_coding_block_size_minus3: sps.log2_min_pcm_luma_coding_block_size_minus3,
        log2_diff_max_min_pcm_luma_coding_block_size: sps.log2_diff_max_min_pcm_luma_coding_block_size,
        num_short_term_ref_pic_sets: sps.num_short_term_ref_pic_sets,
        num_long_term_ref_pics_sps: sps.num_long_term_ref_pics_sps,
        chroma_format_idc: sps.chroma_format_idc,
        sps_max_sub_layers_minus1: sps.max_sub_layers_minus1,
        reserved: [0; 6],
        flags,
    }
}

fn uapi_pps(pps: &Pps) -> uapi::Pps {
    let mut flags = 0;
    for (on, flag) in [
        (pps.dependent_slice_segments_enabled_flag, uapi::PPS_FLAG_DEPENDENT_SLICE_SEGMENT_ENABLED),
        (pps.output_flag_present_flag, uapi::PPS_FLAG_OUTPUT_FLAG_PRESENT),
        (pps.sign_data_hiding_enabled_flag, uapi::PPS_FLAG_SIGN_DATA_HIDING_ENABLED),
        (pps.cabac_init_present_flag, uapi::PPS_FLAG_CABAC_INIT_PRESENT),
        (pps.constrained_intra_pred_flag, uapi::PPS_FLAG_CONSTRAINED_INTRA_PRED),
        (pps.transform_skip_enabled_flag, uapi::PPS_FLAG_TRANSFORM_SKIP_ENABLED),
        (pps.cu_qp_delta_enabled_flag, uapi::PPS_FLAG_CU_QP_DELTA_ENABLED),
        (pps.slice_chroma_qp_offsets_present_flag, uapi::PPS_FLAG_PPS_SLICE_CHROMA_QP_OFFSETS_PRESENT),
        (pps.weighted_pred_flag, uapi::PPS_FLAG_WEIGHTED_PRED),
        (pps.weighted_bipred_flag, uapi::PPS_FLAG_WEIGHTED_BIPRED),
        (pps.transquant_bypass_enabled_flag, uapi::PPS_FLAG_TRANSQUANT_BYPASS_ENABLED),
        (pps.tiles_enabled_flag, uapi::PPS_FLAG_TILES_ENABLED),
        (pps.entropy_coding_sync_enabled_flag, uapi::PPS_FLAG_ENTROPY_CODING_SYNC_ENABLED),
        (pps.loop_filter_across_tiles_enabled_flag, uapi::PPS_FLAG_LOOP_FILTER_ACROSS_TILES_ENABLED),
        (pps.loop_filter_across_slices_enabled_flag, uapi::PPS_FLAG_PPS_LOOP_FILTER_ACROSS_SLICES_ENABLED),
        (pps.deblocking_filter_override_enabled_flag, uapi::PPS_FLAG_DEBLOCKING_FILTER_OVERRIDE_ENABLED),
        (pps.deblocking_filter_disabled_flag, uapi::PPS_FLAG_PPS_DISABLE_DEBLOCKING_FILTER),
        (pps.lists_modification_present_flag, uapi::PPS_FLAG_LISTS_MODIFICATION_PRESENT),
        (pps.slice_segment_header_extension_present_flag, uapi::PPS_FLAG_SLICE_SEGMENT_HEADER_EXTENSION_PRESENT),
        (pps.deblocking_filter_control_present_flag, uapi::PPS_FLAG_DEBLOCKING_FILTER_CONTROL_PRESENT),
        (pps.uniform_spacing_flag, uapi::PPS_FLAG_UNIFORM_SPACING),
    ] {
        if on {
            flags |= flag;
        }
    }
    let mut out = uapi::Pps {
        pic_parameter_set_id: pps.pic_parameter_set_id,
        num_extra_slice_header_bits: pps.num_extra_slice_header_bits,
        num_ref_idx_l0_default_active_minus1: pps.num_ref_idx_l0_default_active_minus1,
        num_ref_idx_l1_default_active_minus1: pps.num_ref_idx_l1_default_active_minus1,
        init_qp_minus26: pps.init_qp_minus26,
        diff_cu_qp_delta_depth: pps.diff_cu_qp_delta_depth,
        pps_cb_qp_offset: pps.cb_qp_offset,
        pps_cr_qp_offset: pps.cr_qp_offset,
        num_tile_columns_minus1: pps.num_tile_columns_minus1,
        num_tile_rows_minus1: pps.num_tile_rows_minus1,
        pps_beta_offset_div2: pps.beta_offset_div2,
        pps_tc_offset_div2: pps.tc_offset_div2,
        log2_parallel_merge_level_minus2: pps.log2_parallel_merge_level_minus2,
        flags,
        ..Default::default()
    };
    for (dst, src) in out.column_width_minus1.iter_mut().zip(&pps.column_width_minus1) {
        *dst = *src as u8;
    }
    for (dst, src) in out.row_height_minus1.iter_mut().zip(&pps.row_height_minus1) {
        *dst = *src as u8;
    }
    out
}

/// The kernel's weight table. Chroma offsets are the derived ChromaOffset
/// values (7.4.7.3), which is what the Raspberry Pi driver programs.
fn uapi_weights(w: &PredWeightTable, sps: &Sps) -> uapi::PredWeightTable {
    let mut out = uapi::PredWeightTable {
        luma_log2_weight_denom: w.luma_log2_weight_denom,
        delta_chroma_log2_weight_denom: w.delta_chroma_log2_weight_denom,
        ..Default::default()
    };
    let half = sps.wp_offset_half_range_c as i32;
    let chroma_denom = w.chroma_log2_weight_denom as u32;
    let chroma_offset = |delta_weight: i8, delta_offset: i16| -> i8 {
        let weight = (1 << chroma_denom) + delta_weight as i32;
        let offset = half + delta_offset as i32 - ((half * weight) >> chroma_denom);
        offset.clamp(-half, half - 1).clamp(-128, 127) as i8
    };
    for i in 0..15 {
        if w.luma_weight_l0_flag[i] {
            out.delta_luma_weight_l0[i] = w.delta_luma_weight_l0[i];
            out.luma_offset_l0[i] = w.luma_offset_l0[i];
        }
        if w.luma_weight_l1_flag[i] {
            out.delta_luma_weight_l1[i] = w.delta_luma_weight_l1[i];
            out.luma_offset_l1[i] = w.luma_offset_l1[i];
        }
        for j in 0..2 {
            if w.chroma_weight_l0_flag[i] {
                out.delta_chroma_weight_l0[i][j] = w.delta_chroma_weight_l0[i][j];
                out.chroma_offset_l0[i][j] = chroma_offset(w.delta_chroma_weight_l0[i][j], w.delta_chroma_offset_l0[i][j]);
            }
            if w.chroma_weight_l1_flag[i] {
                out.delta_chroma_weight_l1[i][j] = w.delta_chroma_weight_l1[i][j];
                out.chroma_offset_l1[i][j] = chroma_offset(w.delta_chroma_weight_l1[i][j], w.delta_chroma_offset_l1[i][j]);
            }
        }
    }
    out
}

/// The five reference picture set lists for one picture (8.3.2), as POCs.
#[derive(Debug, Default, PartialEq)]
struct Rps {
    st_curr_before: Vec<i32>,
    st_curr_after: Vec<i32>,
    st_foll: Vec<i32>,
    /// (poc or poc lsb, full poc known, used by current picture)
    lt: Vec<(i32, bool, bool)>,
}

fn reference_picture_set(hdr: &SliceHeader, sps: &Sps, poc: i32) -> Rps {
    let mut rps = Rps::default();
    let st: &ShortTermRefPicSet = if hdr.short_term_ref_pic_set_sps_flag {
        &sps.short_term_ref_pic_set[hdr.short_term_ref_pic_set_idx as usize]
    } else {
        &hdr.short_term_ref_pic_set
    };
    for i in 0..st.num_negative_pics as usize {
        let p = poc + st.delta_poc_s0[i];
        if st.used_by_curr_pic_s0[i] { rps.st_curr_before.push(p) } else { rps.st_foll.push(p) }
    }
    for i in 0..st.num_positive_pics as usize {
        let p = poc + st.delta_poc_s1[i];
        if st.used_by_curr_pic_s1[i] { rps.st_curr_after.push(p) } else { rps.st_foll.push(p) }
    }
    let max_lsb = 1i32 << (sps.log2_max_pic_order_cnt_lsb_minus4 + 4);
    let total = hdr.num_long_term_sps as usize + hdr.num_long_term_pics as usize;
    for i in 0..total {
        let (lsb, used) = if i < hdr.num_long_term_sps as usize {
            let k = hdr.lt_idx_sps[i] as usize;
            (sps.lt_ref_pic_poc_lsb_sps[k] as i32, sps.used_by_curr_pic_lt_sps_flag[k])
        } else {
            (hdr.poc_lsb_lt[i] as i32, hdr.used_by_curr_pic_lt[i])
        };
        if hdr.delta_poc_msb_present_flag[i] {
            // The parser already accumulates DeltaPocMsbCycleLt (7-52).
            let full = poc - hdr.delta_poc_msb_cycle_lt[i] as i32 * max_lsb - (poc & (max_lsb - 1)) + lsb;
            rps.lt.push((full, true, used));
        } else {
            rps.lt.push((lsb, false, used));
        }
    }
    rps
}

/// 8.3.4: a reference picture list as indices into `curr` (the picture's
/// StCurrBefore/After and LtCurr references, already resolved).
fn ref_pic_list(temp_order: &[u8], num_active: usize, num_total: usize, modification: Option<&[u32]>) -> Vec<u8> {
    if temp_order.is_empty() {
        return Vec::new();
    }
    let len = num_active.max(num_total);
    let temp: Vec<u8> = temp_order.iter().copied().cycle().take(len).collect();
    (0..num_active).map(|i| modification.map_or(temp[i], |m| temp[m[i] as usize % temp.len()])).collect()
}

impl HevcFrames {
    pub fn new(hvcc: &[u8], nal_length_size: usize) -> Result<Self> {
        let mut frames = Self {
            parser: Parser::default(),
            nal_length_size,
            dpb: Vec::new(),
            prev_tid0_poc: 0,
            new_sequence: true,
            waiting_for_irap: true,
            skip_rasl: false,
            next_tag: 1,
        };
        let stream = annex_b(hvcc_nal_units(hvcc)?);
        frames.parse_parameter_sets(&stream)?;
        Ok(frames)
    }

    fn parse_parameter_sets(&mut self, annexb: &[u8]) -> Result<()> {
        let mut cursor = Cursor::new(annexb);
        while let Ok(nalu) = Nalu::next(&mut cursor) {
            match nalu.header.type_ {
                NaluType::VpsNut => drop(self.parser.parse_vps(&nalu).map_err(|e| anyhow!("VPS: {e}"))?),
                NaluType::SpsNut => drop(self.parser.parse_sps(&nalu).map_err(|e| anyhow!("SPS: {e}"))?),
                NaluType::PpsNut => drop(self.parser.parse_pps(&nalu).map_err(|e| anyhow!("PPS: {e}"))?),
                _ => {}
            }
        }
        Ok(())
    }

    /// After a seek: decoding restarts at the next IRAP picture.
    pub fn reset(&mut self) {
        self.dpb.clear();
        self.new_sequence = true;
        self.waiting_for_irap = true;
        self.skip_rasl = false;
    }

    /// Build the controls for one access unit, or `None` if the picture is
    /// skipped (before the first IRAP, an undecodable RASL picture, or a
    /// missing reference).
    pub fn frame(&mut self, access_unit: &[u8]) -> Result<Option<FrameControls>> {
        let annexb = annex_b(length_prefixed(access_unit, self.nal_length_size));
        let mut cursor = Cursor::new(annexb.as_slice());
        let mut slices: Vec<Slice<'_>> = Vec::new();
        while let Ok(nalu) = Nalu::next(&mut cursor) {
            let t = nalu.header.type_;
            match t {
                NaluType::VpsNut => drop(self.parser.parse_vps(&nalu).map_err(|e| anyhow!("VPS: {e}"))?),
                NaluType::SpsNut => drop(self.parser.parse_sps(&nalu).map_err(|e| anyhow!("SPS: {e}"))?),
                NaluType::PpsNut => drop(self.parser.parse_pps(&nalu).map_err(|e| anyhow!("PPS: {e}"))?),
                NaluType::EosNut => self.new_sequence = true,
                _ if (t as u32) <= 21 => {
                    slices.push(self.parser.parse_slice_header(nalu).map_err(|e| anyhow!("slice header: {e}"))?)
                }
                _ => {}
            }
        }
        let Some(first) = slices.first() else { return Ok(None) };
        let nal_type = first.nalu.header.type_;
        let temporal_id_plus1 = first.nalu.header.nuh_temporal_id_plus1;
        let hdr = &first.header;

        let irap = nal_type.is_irap();
        if self.waiting_for_irap && !irap {
            return Ok(None);
        }
        let no_rasl_output = irap && (nal_type.is_idr() || nal_type.is_bla() || self.new_sequence);
        if irap {
            self.waiting_for_irap = false;
            self.new_sequence = false;
            self.skip_rasl = no_rasl_output;
        }
        if nal_type.is_rasl() && self.skip_rasl {
            return Ok(None);
        }

        let pps: Rc<Pps> = self
            .parser
            .get_pps(hdr.pic_parameter_set_id)
            .cloned()
            .ok_or_else(|| anyhow!("slice references unknown PPS {}", hdr.pic_parameter_set_id))?;
        let sps: Rc<Sps> = pps.sps.clone();

        // 8.3.1: picture order count.
        let max_lsb = 1i32 << (sps.log2_max_pic_order_cnt_lsb_minus4 + 4);
        let lsb = hdr.pic_order_cnt_lsb as i32;
        let msb = if irap && no_rasl_output {
            0
        } else {
            let prev_lsb = self.prev_tid0_poc & (max_lsb - 1);
            let prev_msb = self.prev_tid0_poc - prev_lsb;
            if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
                prev_msb + max_lsb
            } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
                prev_msb - max_lsb
            } else {
                prev_msb
            }
        };
        let poc = msb + lsb;
        if temporal_id_plus1 == 1 && !nal_type.is_rasl() && !nal_type.is_radl() && !is_sub_layer_non_reference(nal_type) {
            self.prev_tid0_poc = poc;
        }

        // 8.3.2: reference picture set; everything else leaves the DPB.
        if irap && no_rasl_output {
            self.dpb.clear();
        }
        let rps = if nal_type.is_idr() { Rps::default() } else { reference_picture_set(hdr, &sps, poc) };
        let find_st = |dpb: &[RefPic], p: i32| dpb.iter().position(|r| r.poc == p && !r.long_term);
        let find_lt = |dpb: &[RefPic], (p, full, _): (i32, bool, bool)| {
            dpb.iter().position(|r| if full { r.poc == p } else { r.poc & (max_lsb - 1) == p })
        };
        let mut keep: Vec<RefPic> = Vec::new();
        let mut take = |dpb: &[RefPic], at: Option<usize>, long_term: bool| -> Option<usize> {
            let r = dpb[at?];
            if let Some(k) = keep.iter().position(|e| e.tag == r.tag) {
                keep[k].long_term |= long_term;
                return Some(k);
            }
            keep.push(RefPic { long_term, ..r });
            Some(keep.len() - 1)
        };
        let mut missing = false;
        let mut resolve = |list: &[i32], keep_fn: &mut dyn FnMut(Option<usize>) -> Option<usize>, used: bool| -> Vec<u8> {
            list.iter()
                .filter_map(|&p| {
                    let k = keep_fn(find_st(&self.dpb, p));
                    if k.is_none() && used {
                        missing = true;
                    }
                    k.map(|k| k as u8)
                })
                .collect()
        };
        let before = resolve(&rps.st_curr_before, &mut |at| take(&self.dpb, at, false), true);
        let after = resolve(&rps.st_curr_after, &mut |at| take(&self.dpb, at, false), true);
        let _foll = resolve(&rps.st_foll, &mut |at| take(&self.dpb, at, false), false);
        let mut lt_curr = Vec::new();
        for &entry in &rps.lt {
            let k = take(&self.dpb, find_lt(&self.dpb, entry), true);
            match (k, entry.2) {
                (Some(k), true) => lt_curr.push(k as u8),
                (None, true) => missing = true,
                _ => {}
            }
        }
        // Sequences starting at a CRA list references for their skipped
        // RASL pictures; the CRA itself is intra, so only flag real gaps.
        if missing && !irap {
            log::debug!("HEVC picture POC {poc} references a picture not in the DPB; skipped");
            return Ok(None);
        }
        let live_tags: Vec<u64> = keep.iter().map(|r| r.tag).collect();

        let mut decode = uapi::DecodeParams {
            pic_order_cnt_val: poc,
            short_term_ref_pic_set_size: if hdr.short_term_ref_pic_set_sps_flag { 0 } else { hdr.st_rps_bits as u16 },
            num_active_dpb_entries: keep.len() as u8,
            num_poc_st_curr_before: before.len() as u8,
            num_poc_st_curr_after: after.len() as u8,
            num_poc_lt_curr: lt_curr.len() as u8,
            ..Default::default()
        };
        decode.poc_st_curr_before[..before.len()].copy_from_slice(&before);
        decode.poc_st_curr_after[..after.len()].copy_from_slice(&after);
        decode.poc_lt_curr[..lt_curr.len()].copy_from_slice(&lt_curr);
        for (dst, r) in decode.dpb.iter_mut().zip(&keep) {
            *dst = uapi::DpbEntry {
                timestamp: tag_to_ns(r.tag),
                flags: if r.long_term { uapi::DPB_ENTRY_LONG_TERM_REFERENCE } else { 0 },
                pic_order_cnt_val: r.poc,
                ..Default::default()
            };
        }
        let st_set = if hdr.short_term_ref_pic_set_sps_flag {
            &sps.short_term_ref_pic_set[hdr.short_term_ref_pic_set_idx as usize]
        } else {
            &hdr.short_term_ref_pic_set
        };
        if st_set.inter_ref_pic_set_prediction_flag {
            let idx = if hdr.short_term_ref_pic_set_sps_flag {
                hdr.short_term_ref_pic_set_idx as usize
            } else {
                sps.num_short_term_ref_pic_sets as usize
            };
            let ref_idx = idx.saturating_sub(st_set.delta_idx_minus1 as usize + 1);
            decode.num_delta_pocs_of_ref_rps_idx =
                sps.short_term_ref_pic_set.get(ref_idx).map_or(0, |s| s.num_delta_pocs as u8);
        }
        if irap {
            decode.flags |= uapi::DECODE_FLAG_IRAP_PIC;
        }
        if nal_type.is_idr() {
            decode.flags |= uapi::DECODE_FLAG_IDR_PIC;
        }
        if hdr.no_output_of_prior_pics_flag {
            decode.flags |= uapi::DECODE_FLAG_NO_OUTPUT_OF_PRIOR;
        }

        // Slices: NAL units back to back; offsets point at each slice's data.
        let num_total = before.len() + after.len() + lt_curr.len();
        let l0_order: Vec<u8> = before.iter().chain(&after).chain(&lt_curr).copied().collect();
        let l1_order: Vec<u8> = after.iter().chain(&before).chain(&lt_curr).copied().collect();
        let mut data = Vec::new();
        let mut params = Vec::with_capacity(slices.len());
        for slice in &slices {
            let h = &slice.header;
            let nal = &slice.nalu.data[slice.nalu.offset..slice.nalu.offset + slice.nalu.size];
            let nal_start = data.len();
            data.extend_from_slice(nal);
            let header_bytes = (h.header_bit_size / 8 + h.n_emulation_prevention_bytes) as usize;
            if header_bytes > nal.len() {
                bail!("HEVC slice header ({header_bytes} bytes) longer than its NAL unit ({})", nal.len());
            }
            let mut p = uapi::SliceParams {
                bit_size: ((nal.len() - header_bytes) * 8) as u32,
                data_byte_offset: (nal_start + header_bytes) as u32,
                num_entry_point_offsets: h.num_entry_point_offsets,
                nal_unit_type: slice.nalu.header.type_ as u8,
                nuh_temporal_id_plus1: slice.nalu.header.nuh_temporal_id_plus1,
                slice_type: h.type_ as u8,
                colour_plane_id: h.colour_plane_id,
                slice_pic_order_cnt: poc,
                collocated_ref_idx: h.collocated_ref_idx,
                five_minus_max_num_merge_cand: h.five_minus_max_num_merge_cand,
                slice_qp_delta: h.qp_delta,
                slice_cb_qp_offset: h.cb_qp_offset,
                slice_cr_qp_offset: h.cr_qp_offset,
                slice_act_y_qp_offset: h.slice_act_y_qp_offset,
                slice_act_cb_qp_offset: h.slice_act_cb_qp_offset,
                slice_act_cr_qp_offset: h.slice_act_cr_qp_offset,
                slice_beta_offset_div2: h.beta_offset_div2,
                slice_tc_offset_div2: h.tc_offset_div2,
                slice_segment_addr: h.segment_address,
                short_term_ref_pic_set_size: if h.short_term_ref_pic_set_sps_flag { 0 } else { h.st_rps_bits as u16 },
                pred_weight_table: uapi_weights(&h.pred_weight_table, &sps),
                ..Default::default()
            };
            let modification = &h.ref_pic_list_modification;
            if h.type_.is_p() || h.type_.is_b() {
                p.num_ref_idx_l0_active_minus1 = h.num_ref_idx_l0_active_minus1;
                let n0 = h.num_ref_idx_l0_active_minus1 as usize + 1;
                let mods0 = modification.ref_pic_list_modification_flag_l0.then_some(modification.list_entry_l0.as_slice());
                for (i, k) in ref_pic_list(&l0_order, n0, num_total, mods0).into_iter().enumerate() {
                    p.ref_idx_l0[i] = k;
                }
            }
            if h.type_.is_b() {
                p.num_ref_idx_l1_active_minus1 = h.num_ref_idx_l1_active_minus1;
                let n1 = h.num_ref_idx_l1_active_minus1 as usize + 1;
                let mods1 = modification.ref_pic_list_modification_flag_l1.then_some(modification.list_entry_l1.as_slice());
                for (i, k) in ref_pic_list(&l1_order, n1, num_total, mods1).into_iter().enumerate() {
                    p.ref_idx_l1[i] = k;
                }
            }
            for (on, flag) in [
                (h.sao_luma_flag, uapi::SLICE_FLAG_SAO_LUMA),
                (h.sao_chroma_flag, uapi::SLICE_FLAG_SAO_CHROMA),
                (h.temporal_mvp_enabled_flag, uapi::SLICE_FLAG_TEMPORAL_MVP_ENABLED),
                (h.mvd_l1_zero_flag, uapi::SLICE_FLAG_MVD_L1_ZERO),
                (h.cabac_init_flag, uapi::SLICE_FLAG_CABAC_INIT),
                (h.collocated_from_l0_flag, uapi::SLICE_FLAG_COLLOCATED_FROM_L0),
                (h.use_integer_mv_flag, uapi::SLICE_FLAG_USE_INTEGER_MV),
                (h.deblocking_filter_disabled_flag, uapi::SLICE_FLAG_DEBLOCKING_FILTER_DISABLED),
                (h.loop_filter_across_slices_enabled_flag, uapi::SLICE_FLAG_LOOP_FILTER_ACROSS_SLICES_ENABLED),
                (h.dependent_slice_segment_flag, uapi::SLICE_FLAG_DEPENDENT_SLICE_SEGMENT),
            ] {
                if on {
                    p.flags |= flag;
                }
            }
            params.push(p);
        }

        let scaling = sps.scaling_list_enabled_flag.then(|| {
            scaling_matrix(if pps.scaling_list_data_present_flag { &pps.scaling_list } else { &sps.scaling_list })
        });

        // This picture joins the DPB for the pictures after it.
        let tag = self.next_tag;
        self.next_tag += 1;
        keep.push(RefPic { poc, tag, long_term: false });
        self.dpb = keep;

        let top = sps.max_sub_layers_minus1 as usize;
        Ok(Some(FrameControls {
            sps: uapi_sps(&sps),
            pps: uapi_pps(&pps),
            scaling,
            slices: params,
            decode,
            data,
            tag,
            live_tags,
            width: sps.pic_width_in_luma_samples as u32,
            height: sps.pic_height_in_luma_samples as u32,
            bit_depth: sps.bit_depth_luma_minus8 + 8,
            max_dpb: sps.max_dec_pic_buffering_minus1[top] as usize + 1,
        }))
    }
}

/// Tags travel as V4L2 buffer timestamps: microseconds in a `timeval`, which
/// the kernel compares in nanoseconds.
pub fn tag_to_ns(tag: u64) -> u64 {
    tag * 1000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn up_right_diagonal_matches_the_specification() {
        // H.265 6.5.3 for a 4x4 block: (x, y) pairs in scan order.
        let order = up_right_diagonal(4);
        assert_eq!(&order[..6], &[(0, 0), (0, 1), (1, 0), (0, 2), (1, 1), (2, 0)]);
        assert_eq!(order[15], (3, 3));
        let coded: [u8; 16] = std::array::from_fn(|i| i as u8);
        let raster = to_raster(&coded, 4);
        // Raster row 0 holds scan positions 0, 2, 5, 9.
        assert_eq!(&raster[..4], &[0, 2, 5, 9]);
    }

    #[test]
    fn reference_lists_repeat_and_apply_modifications() {
        // Two references, three active entries: the list wraps (8-8).
        assert_eq!(ref_pic_list(&[4, 7], 3, 2, None), vec![4, 7, 4]);
        // Modification picks entries by index into the temporary list.
        assert_eq!(ref_pic_list(&[4, 7], 2, 2, Some(&[1, 0])), vec![7, 4]);
        assert!(ref_pic_list(&[], 1, 0, None).is_empty());
    }

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)).unwrap()
    }

    /// Feed a real x265 stream (B-frames, 3 reorder) through the state
    /// machine and check POCs and references against its structure.
    #[test]
    fn x265_b_frame_stream_produces_consistent_controls() {
        use symphonia::core::formats::probe::Hint;
        use symphonia::core::formats::{FormatOptions, TrackType};
        use symphonia::core::io::MediaSourceStream;
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bframes_hevc.mkv");
        let mss = MediaSourceStream::new(Box::new(std::fs::File::open(&path).unwrap()), Default::default());
        let mut format = symphonia::default::get_probe()
            .probe(Hint::new().with_extension("mkv"), mss, FormatOptions::default(), Default::default())
            .unwrap();
        let track = format.default_track(TrackType::Video).unwrap().clone();
        let params = track.codec_params.as_ref().unwrap().video().unwrap();
        let hvcc = &params.extra_data.iter().find(|e| e.data.len() > 22).unwrap().data;
        let mut frames = HevcFrames::new(hvcc, 4).unwrap();
        let mut pocs = Vec::new();
        while let Some(packet) = format.next_packet().unwrap() {
            if packet.track_id != track.id {
                continue;
            }
            let f = frames.frame(&packet.data).unwrap().expect("every picture decodable from the start");
            assert_eq!((f.width, f.height, f.bit_depth), (160, 120, 8));
            assert_eq!(f.slices.len(), 1);
            let s = &f.slices[0];
            // Slice data starts inside the NAL, past a header of sane size.
            assert!(s.data_byte_offset >= 3 && (s.data_byte_offset as usize) < f.data.len());
            assert_eq!(s.bit_size as usize, (f.data.len() - s.data_byte_offset as usize) * 8);
            let d = &f.decode;
            // Every reference index points inside the DPB array.
            let n = d.num_active_dpb_entries;
            for &k in d.poc_st_curr_before[..d.num_poc_st_curr_before as usize].iter()
                .chain(&d.poc_st_curr_after[..d.num_poc_st_curr_after as usize])
            {
                assert!(k < n, "ref index {k} >= {n}");
            }
            // Before-refs precede the picture, after-refs follow it.
            for &k in &d.poc_st_curr_before[..d.num_poc_st_curr_before as usize] {
                assert!(d.dpb[k as usize].pic_order_cnt_val < d.pic_order_cnt_val);
            }
            for &k in &d.poc_st_curr_after[..d.num_poc_st_curr_after as usize] {
                assert!(d.dpb[k as usize].pic_order_cnt_val > d.pic_order_cnt_val);
            }
            if s.slice_type == uapi::SLICE_TYPE_B {
                assert!(d.num_poc_st_curr_before + d.num_poc_st_curr_after > 0, "B picture without references");
            }
            assert!(f.live_tags.len() <= f.max_dpb);
            pocs.push(d.pic_order_cnt_val);
        }
        assert_eq!(pocs.len(), 50);
        // Display order is a permutation of 0..50 (two 25-frame GOPs, POC
        // restarting at each IDR) and decode order differs from it.
        let mut sorted = pocs.clone();
        sorted.sort();
        assert_ne!(pocs, sorted, "B-frames reorder");
        assert_eq!(pocs[0], 0);
    }

    #[test]
    fn parameter_sets_from_the_hevc_fixture_record() {
        let bytes = fixture("bframes_hevc.mkv");
        let at = bytes.windows(2).position(|w| w == [0x63, 0xa2]).unwrap();
        let len = (bytes[at + 2] & 0x7f) as usize;
        let hvcc = &bytes[at + 3..at + 3 + len];
        let nals = hvcc_nal_units(hvcc).unwrap();
        assert_eq!(nals.len(), 3, "VPS, SPS, PPS");
        assert!(HevcFrames::new(hvcc, 4).is_ok());
    }
}
