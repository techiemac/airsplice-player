//! The Linux stateless HEVC decoding controls (`linux/v4l2-controls.h`,
//! stable uapi since Linux 6.0), mirrored with identical `repr(C)` layouts.
//!
//! They live here, not only as bindgen output, so the HEVC logic that fills
//! them is portable and unit-testable on any OS. The Linux device code
//! asserts at compile time that each size matches the kernel headers'.

pub const DPB_ENTRIES_NUM_MAX: usize = 16;

pub const SPS_FLAG_SEPARATE_COLOUR_PLANE: u64 = 1 << 0;
pub const SPS_FLAG_SCALING_LIST_ENABLED: u64 = 1 << 1;
pub const SPS_FLAG_AMP_ENABLED: u64 = 1 << 2;
pub const SPS_FLAG_SAMPLE_ADAPTIVE_OFFSET: u64 = 1 << 3;
pub const SPS_FLAG_PCM_ENABLED: u64 = 1 << 4;
pub const SPS_FLAG_PCM_LOOP_FILTER_DISABLED: u64 = 1 << 5;
pub const SPS_FLAG_LONG_TERM_REF_PICS_PRESENT: u64 = 1 << 6;
pub const SPS_FLAG_SPS_TEMPORAL_MVP_ENABLED: u64 = 1 << 7;
pub const SPS_FLAG_STRONG_INTRA_SMOOTHING_ENABLED: u64 = 1 << 8;

pub const PPS_FLAG_DEPENDENT_SLICE_SEGMENT_ENABLED: u64 = 1 << 0;
pub const PPS_FLAG_OUTPUT_FLAG_PRESENT: u64 = 1 << 1;
pub const PPS_FLAG_SIGN_DATA_HIDING_ENABLED: u64 = 1 << 2;
pub const PPS_FLAG_CABAC_INIT_PRESENT: u64 = 1 << 3;
pub const PPS_FLAG_CONSTRAINED_INTRA_PRED: u64 = 1 << 4;
pub const PPS_FLAG_TRANSFORM_SKIP_ENABLED: u64 = 1 << 5;
pub const PPS_FLAG_CU_QP_DELTA_ENABLED: u64 = 1 << 6;
pub const PPS_FLAG_PPS_SLICE_CHROMA_QP_OFFSETS_PRESENT: u64 = 1 << 7;
pub const PPS_FLAG_WEIGHTED_PRED: u64 = 1 << 8;
pub const PPS_FLAG_WEIGHTED_BIPRED: u64 = 1 << 9;
pub const PPS_FLAG_TRANSQUANT_BYPASS_ENABLED: u64 = 1 << 10;
pub const PPS_FLAG_TILES_ENABLED: u64 = 1 << 11;
pub const PPS_FLAG_ENTROPY_CODING_SYNC_ENABLED: u64 = 1 << 12;
pub const PPS_FLAG_LOOP_FILTER_ACROSS_TILES_ENABLED: u64 = 1 << 13;
pub const PPS_FLAG_PPS_LOOP_FILTER_ACROSS_SLICES_ENABLED: u64 = 1 << 14;
pub const PPS_FLAG_DEBLOCKING_FILTER_OVERRIDE_ENABLED: u64 = 1 << 15;
pub const PPS_FLAG_PPS_DISABLE_DEBLOCKING_FILTER: u64 = 1 << 16;
pub const PPS_FLAG_LISTS_MODIFICATION_PRESENT: u64 = 1 << 17;
pub const PPS_FLAG_SLICE_SEGMENT_HEADER_EXTENSION_PRESENT: u64 = 1 << 18;
pub const PPS_FLAG_DEBLOCKING_FILTER_CONTROL_PRESENT: u64 = 1 << 19;
pub const PPS_FLAG_UNIFORM_SPACING: u64 = 1 << 20;

pub const SLICE_FLAG_SAO_LUMA: u64 = 1 << 0;
pub const SLICE_FLAG_SAO_CHROMA: u64 = 1 << 1;
pub const SLICE_FLAG_TEMPORAL_MVP_ENABLED: u64 = 1 << 2;
pub const SLICE_FLAG_MVD_L1_ZERO: u64 = 1 << 3;
pub const SLICE_FLAG_CABAC_INIT: u64 = 1 << 4;
pub const SLICE_FLAG_COLLOCATED_FROM_L0: u64 = 1 << 5;
pub const SLICE_FLAG_USE_INTEGER_MV: u64 = 1 << 6;
pub const SLICE_FLAG_DEBLOCKING_FILTER_DISABLED: u64 = 1 << 7;
pub const SLICE_FLAG_LOOP_FILTER_ACROSS_SLICES_ENABLED: u64 = 1 << 8;
pub const SLICE_FLAG_DEPENDENT_SLICE_SEGMENT: u64 = 1 << 9;

pub const DECODE_FLAG_IRAP_PIC: u64 = 0x1;
pub const DECODE_FLAG_IDR_PIC: u64 = 0x2;
pub const DECODE_FLAG_NO_OUTPUT_OF_PRIOR: u64 = 0x4;

pub const DPB_ENTRY_LONG_TERM_REFERENCE: u8 = 0x01;

/// `V4L2_HEVC_SLICE_TYPE_{B,P,I}`.
pub const SLICE_TYPE_B: u8 = 0;
pub const SLICE_TYPE_P: u8 = 1;
pub const SLICE_TYPE_I: u8 = 2;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Sps {
    pub video_parameter_set_id: u8,
    pub seq_parameter_set_id: u8,
    pub pic_width_in_luma_samples: u16,
    pub pic_height_in_luma_samples: u16,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub log2_max_pic_order_cnt_lsb_minus4: u8,
    pub sps_max_dec_pic_buffering_minus1: u8,
    pub sps_max_num_reorder_pics: u8,
    pub sps_max_latency_increase_plus1: u8,
    pub log2_min_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_luma_coding_block_size: u8,
    pub log2_min_luma_transform_block_size_minus2: u8,
    pub log2_diff_max_min_luma_transform_block_size: u8,
    pub max_transform_hierarchy_depth_inter: u8,
    pub max_transform_hierarchy_depth_intra: u8,
    pub pcm_sample_bit_depth_luma_minus1: u8,
    pub pcm_sample_bit_depth_chroma_minus1: u8,
    pub log2_min_pcm_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_pcm_luma_coding_block_size: u8,
    pub num_short_term_ref_pic_sets: u8,
    pub num_long_term_ref_pics_sps: u8,
    pub chroma_format_idc: u8,
    pub sps_max_sub_layers_minus1: u8,
    pub reserved: [u8; 6],
    pub flags: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Pps {
    pub pic_parameter_set_id: u8,
    pub num_extra_slice_header_bits: u8,
    pub num_ref_idx_l0_default_active_minus1: u8,
    pub num_ref_idx_l1_default_active_minus1: u8,
    pub init_qp_minus26: i8,
    pub diff_cu_qp_delta_depth: u8,
    pub pps_cb_qp_offset: i8,
    pub pps_cr_qp_offset: i8,
    pub num_tile_columns_minus1: u8,
    pub num_tile_rows_minus1: u8,
    pub column_width_minus1: [u8; 20],
    pub row_height_minus1: [u8; 22],
    pub pps_beta_offset_div2: i8,
    pub pps_tc_offset_div2: i8,
    pub log2_parallel_merge_level_minus2: u8,
    pub reserved: u8,
    pub flags: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DpbEntry {
    /// The capture buffer holding the reference, identified by the
    /// timestamp (ns) of the output buffer it was decoded from.
    pub timestamp: u64,
    pub flags: u8,
    pub field_pic: u8,
    pub reserved: u16,
    pub pic_order_cnt_val: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PredWeightTable {
    pub delta_luma_weight_l0: [i8; DPB_ENTRIES_NUM_MAX],
    pub luma_offset_l0: [i8; DPB_ENTRIES_NUM_MAX],
    pub delta_chroma_weight_l0: [[i8; 2]; DPB_ENTRIES_NUM_MAX],
    pub chroma_offset_l0: [[i8; 2]; DPB_ENTRIES_NUM_MAX],
    pub delta_luma_weight_l1: [i8; DPB_ENTRIES_NUM_MAX],
    pub luma_offset_l1: [i8; DPB_ENTRIES_NUM_MAX],
    pub delta_chroma_weight_l1: [[i8; 2]; DPB_ENTRIES_NUM_MAX],
    pub chroma_offset_l1: [[i8; 2]; DPB_ENTRIES_NUM_MAX],
    pub luma_log2_weight_denom: u8,
    pub delta_chroma_log2_weight_denom: i8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SliceParams {
    /// Size in bits of the slice data that starts at `data_byte_offset`.
    pub bit_size: u32,
    /// Offset in bytes, from the start of the output buffer, of this
    /// slice's data (just past its header).
    pub data_byte_offset: u32,
    pub num_entry_point_offsets: u32,
    pub nal_unit_type: u8,
    pub nuh_temporal_id_plus1: u8,
    pub slice_type: u8,
    pub colour_plane_id: u8,
    pub slice_pic_order_cnt: i32,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub collocated_ref_idx: u8,
    pub five_minus_max_num_merge_cand: u8,
    pub slice_qp_delta: i8,
    pub slice_cb_qp_offset: i8,
    pub slice_cr_qp_offset: i8,
    pub slice_act_y_qp_offset: i8,
    pub slice_act_cb_qp_offset: i8,
    pub slice_act_cr_qp_offset: i8,
    pub slice_beta_offset_div2: i8,
    pub slice_tc_offset_div2: i8,
    pub pic_struct: u8,
    pub reserved0: [u8; 3],
    pub slice_segment_addr: u32,
    pub ref_idx_l0: [u8; DPB_ENTRIES_NUM_MAX],
    pub ref_idx_l1: [u8; DPB_ENTRIES_NUM_MAX],
    pub short_term_ref_pic_set_size: u16,
    pub long_term_ref_pic_set_size: u16,
    pub pred_weight_table: PredWeightTable,
    pub reserved1: [u8; 2],
    pub flags: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DecodeParams {
    pub pic_order_cnt_val: i32,
    pub short_term_ref_pic_set_size: u16,
    pub long_term_ref_pic_set_size: u16,
    pub num_active_dpb_entries: u8,
    pub num_poc_st_curr_before: u8,
    pub num_poc_st_curr_after: u8,
    pub num_poc_lt_curr: u8,
    pub poc_st_curr_before: [u8; DPB_ENTRIES_NUM_MAX],
    pub poc_st_curr_after: [u8; DPB_ENTRIES_NUM_MAX],
    pub poc_lt_curr: [u8; DPB_ENTRIES_NUM_MAX],
    pub num_delta_pocs_of_ref_rps_idx: u8,
    pub reserved: [u8; 3],
    pub dpb: [DpbEntry; DPB_ENTRIES_NUM_MAX],
    pub flags: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScalingMatrix {
    /// All lists in raster scan order.
    pub scaling_list_4x4: [[u8; 16]; 6],
    pub scaling_list_8x8: [[u8; 64]; 6],
    pub scaling_list_16x16: [[u8; 64]; 6],
    pub scaling_list_32x32: [[u8; 64]; 2],
    pub scaling_list_dc_coef_16x16: [u8; 6],
    pub scaling_list_dc_coef_32x32: [u8; 2],
}

impl Default for ScalingMatrix {
    fn default() -> Self {
        Self {
            scaling_list_4x4: [[0; 16]; 6],
            scaling_list_8x8: [[0; 64]; 6],
            scaling_list_16x16: [[0; 64]; 6],
            scaling_list_32x32: [[0; 64]; 2],
            scaling_list_dc_coef_16x16: [0; 6],
            scaling_list_dc_coef_32x32: [0; 2],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;

    /// Sizes computed by hand from the uapi header; the Linux build also
    /// checks them against bindgen's output.
    #[test]
    fn layouts_match_the_kernel_uapi() {
        assert_eq!(size_of::<Sps>(), 40);
        assert_eq!(size_of::<Pps>(), 64);
        assert_eq!(size_of::<DpbEntry>(), 16);
        assert_eq!(size_of::<PredWeightTable>(), 194);
        assert_eq!(size_of::<SliceParams>(), 280);
        assert_eq!(size_of::<DecodeParams>(), 328);
        assert_eq!(size_of::<ScalingMatrix>(), 1000);
    }
}
