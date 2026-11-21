//! Loop filter control (RFC 6386 section 15): which edges of which
//! macroblocks are filtered and how strongly. The filters themselves are in
//! `dsp`.

use crate::dsp::{self, EdgeLimits};
use crate::frame::FrameRows;
use crate::header::{FrameHeader, Persistent};
use crate::modes::MbInfo;
use crate::tables::*;

/// Per-frame filter settings.
pub struct LoopFilter {
    simple: bool,
    key_frame: bool,
    sharpness: u8,
    /// Filter level by segment, reference frame and mode class.
    levels: [[[u8; 4]; 4]; 4],
}

/// What the filter needs to know about a macroblock.
#[derive(Clone, Copy)]
pub struct MbFilter {
    segment: u8,
    ref_frame: u8,
    mode: u8,
    /// Inner edges are left alone when the whole macroblock is one
    /// prediction with no residual.
    inner: bool,
}

impl MbFilter {
    pub fn new(mb: &MbInfo) -> Self {
        MbFilter { segment: mb.segment, ref_frame: mb.ref_frame, mode: mb.mode, inner: !(mb.skip && mb.has_y2()) }
    }
}

/// The loop filter delta class of a prediction mode.
fn mode_class(mode: u8) -> usize {
    match mode {
        B_PRED => 0,
        NEARESTMV | NEARMV | NEWMV => 2,
        SPLITMV => 3,
        _ => 1,
    }
}

impl LoopFilter {
    /// The filter for a frame, or `None` when the frame is not filtered.
    pub fn new(header: &FrameHeader, state: &Persistent) -> Option<LoopFilter> {
        if header.filter_level == 0 {
            return None;
        }
        let clamp = |level: i32| level.clamp(0, 63) as u8;
        let levels = std::array::from_fn(|segment| {
            let mut base = i32::from(header.filter_level);
            if header.segmentation {
                let v = i32::from(state.segment_lf[segment]);
                base = i32::from(clamp(if state.segment_abs { v } else { base + v }));
            }
            if !header.lf_deltas {
                return [[base as u8; 4]; 4];
            }
            std::array::from_fn(|ref_frame| {
                let by_ref = base + i32::from(state.ref_lf_deltas[ref_frame]);
                std::array::from_fn(|class| {
                    // Intra macroblocks get a mode delta only for B_PRED
                    // (class 0); inter ones for every class.
                    if ref_frame == 0 && class != 0 {
                        clamp(by_ref)
                    } else {
                        clamp(by_ref + i32::from(state.mode_lf_deltas[class]))
                    }
                })
            })
        });
        Some(LoopFilter { simple: header.simple_filter, key_frame: header.key_frame, sharpness: header.sharpness, levels })
    }

    /// Filter one row of macroblocks. Rows must be filtered top to bottom,
    /// each after it is fully reconstructed; a row's filtering changes up to
    /// three pixel rows of the row above it.
    pub fn filter_row(&self, frame: &mut FrameRows<'_>, mbs: &[MbFilter], mb_row: usize) {
        for (mb_col, mb) in mbs.iter().enumerate() {
            let level = self.levels[usize::from(mb.segment & 3)][usize::from(mb.ref_frame & 3)][mode_class(mb.mode)];
            if level == 0 {
                continue;
            }
            let mut interior = level >> u8::from(self.sharpness > 0) >> u8::from(self.sharpness > 4);
            if self.sharpness > 0 {
                interior = interior.min(9 - self.sharpness);
            }
            let interior = interior.max(1);
            let hev_threshold = match (self.key_frame, level) {
                (true, 40..) => 2,
                (true, 15..) => 1,
                (false, 40..) => 3,
                (false, 20..) => 2,
                (false, 15..) => 1,
                _ => 0,
            };
            let mb_edge = EdgeLimits { edge: 2 * (level + 2) + interior, interior, hev_threshold };
            let inner = EdgeLimits { edge: 2 * level + interior, interior, hev_threshold };
            let filter_inner = mb.inner;

            let y = &mut frame.y;
            let (at, s) = (y.at(mb_col * 16, mb_row * 16), y.stride);
            if self.simple {
                if mb_col > 0 {
                    dsp::simple_edge(y.data, at, 1, s, 16, mb_edge.edge);
                }
                if filter_inner {
                    for x in [4, 8, 12] {
                        dsp::simple_edge(y.data, at + x, 1, s, 16, inner.edge);
                    }
                }
                if mb_row > 0 {
                    dsp::simple_edge(y.data, at, s, 1, 16, mb_edge.edge);
                }
                if filter_inner {
                    for r in [4, 8, 12] {
                        dsp::simple_edge(y.data, at + r * s, s, 1, 16, inner.edge);
                    }
                }
                continue;
            }

            let (u, v) = (&mut frame.u, &mut frame.v);
            let (cat, cs) = (u.at(mb_col * 8, mb_row * 8), u.stride);
            if mb_col > 0 {
                dsp::normal_mb_edge(y.data, at, 1, s, 16, mb_edge);
                dsp::normal_mb_edge(u.data, cat, 1, cs, 8, mb_edge);
                dsp::normal_mb_edge(v.data, cat, 1, cs, 8, mb_edge);
            }
            if filter_inner {
                for x in [4, 8, 12] {
                    dsp::normal_subblock_edge(y.data, at + x, 1, s, 16, inner);
                }
                dsp::normal_subblock_edge(u.data, cat + 4, 1, cs, 8, inner);
                dsp::normal_subblock_edge(v.data, cat + 4, 1, cs, 8, inner);
            }
            if mb_row > 0 {
                dsp::normal_mb_edge(y.data, at, s, 1, 16, mb_edge);
                dsp::normal_mb_edge(u.data, cat, cs, 1, 8, mb_edge);
                dsp::normal_mb_edge(v.data, cat, cs, 1, 8, mb_edge);
            }
            if filter_inner {
                for r in [4, 8, 12] {
                    dsp::normal_subblock_edge(y.data, at + r * s, s, 1, 16, inner);
                }
                dsp::normal_subblock_edge(u.data, cat + 4 * cs, cs, 1, 8, inner);
                dsp::normal_subblock_edge(v.data, cat + 4 * cs, cs, 1, 8, inner);
            }
        }
    }
}
