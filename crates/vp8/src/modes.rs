//! Per-macroblock prediction modes and motion vectors (RFC 6386 sections 8,
//! 10, 11, 16 and 17).

use crate::bool_decoder::BoolDecoder;
use crate::header::{FrameHeader, Probs};
use crate::tables::*;

/// A motion vector in eighths of a luma pixel. The bitstream codes quarter
/// pixels; like libvpx we store them doubled, and in 16 bits with wrapping
/// arithmetic.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Mv {
    pub row: i16,
    pub col: i16,
}

impl Mv {
    pub const ZERO: Mv = Mv { row: 0, col: 0 };
}

pub const INTRA_FRAME: u8 = 0;

#[derive(Clone, Copy)]
pub struct MbInfo {
    /// One of the `tables` mode constants, `DC_PRED` to `SPLITMV`.
    pub mode: u8,
    pub uv_mode: u8,
    /// 0 intra, 1 last, 2 golden, 3 altref.
    pub ref_frame: u8,
    /// Persists across frames that do not update the segment map.
    pub segment: u8,
    /// No coefficients: either flagged in the bitstream or found empty.
    pub skip: bool,
    /// Some motion vector reaches far enough outside the frame that
    /// prediction must clamp it.
    pub need_clamp: bool,
    /// Split layout (index into `MB_SPLITS`) when `mode` is `SPLITMV`.
    pub split: u8,
    /// The macroblock's vector (the last subblock's for `SPLITMV`, zero for
    /// intra).
    pub mv: Mv,
    /// Subblock intra modes. On key frames these are filled in for every
    /// macroblock, since they are the context for the neighbours' modes.
    pub bmodes: [u8; 16],
    /// Subblock vectors, valid when `mode` is `SPLITMV`.
    pub bmvs: [Mv; 16],
}

impl MbInfo {
    /// What lies outside the frame: intra, DC, zero motion.
    pub const BORDER: MbInfo = MbInfo {
        mode: DC_PRED,
        uv_mode: DC_PRED,
        ref_frame: INTRA_FRAME,
        segment: 0,
        skip: false,
        need_clamp: false,
        split: 0,
        mv: Mv::ZERO,
        bmodes: [B_DC_PRED; 16],
        bmvs: [Mv::ZERO; 16],
    };

    /// Whether the luma DC coefficients travel in a separate Y2 block.
    #[inline(always)]
    pub fn has_y2(&self) -> bool {
        self.mode != B_PRED && self.mode != SPLITMV
    }
}

/// How far a macroblock is from each frame edge, in motion vector units,
/// widened by the margin vectors may reach outside the frame.
#[derive(Clone, Copy)]
pub struct MvBounds {
    pub left: i32,
    pub right: i32,
    pub top: i32,
    pub bottom: i32,
}

impl MvBounds {
    /// Distances from macroblock (`mb_col`, `mb_row`) to the frame edges.
    pub fn new(mb_col: usize, mb_row: usize, mb_cols: usize, mb_rows: usize) -> MvBounds {
        MvBounds {
            left: -((mb_col as i32) << 7),
            right: ((mb_cols - 1 - mb_col) as i32) << 7,
            top: -((mb_row as i32) << 7),
            bottom: ((mb_rows - 1 - mb_row) as i32) << 7,
        }
    }

    /// Widen by `before` units on the top and left, `after` on the bottom
    /// and right.
    pub fn widened(self, before: i32, after: i32) -> MvBounds {
        MvBounds {
            left: self.left - before,
            right: self.right + after,
            top: self.top - before,
            bottom: self.bottom + after,
        }
    }

    fn contains(&self, mv: Mv) -> bool {
        (self.left..=self.right).contains(&i32::from(mv.col))
            && (self.top..=self.bottom).contains(&i32::from(mv.row))
    }

    // When a bound lies outside the 16-bit range no vector can exceed it, so
    // the casts below never truncate a value that is used.
    fn clamp(&self, mv: Mv) -> Mv {
        Mv {
            row: i32::from(mv.row).clamp(self.top, self.bottom) as i16,
            col: i32::from(mv.col).clamp(self.left, self.right) as i16,
        }
    }
}

/// Vectors may point up to one macroblock outside the frame before the
/// decoder has to take care.
const MV_MARGIN: i32 = 16 << 3;

fn read_mv_component(bd: &mut BoolDecoder, p: &[u8; MV_PROB_COUNT]) -> i16 {
    let mut x: i32;
    if bd.read(p[MV_IS_SHORT]) {
        x = 0;
        for i in 0..3 {
            x += i32::from(bd.read(p[MV_LONG + i])) << i;
        }
        for i in (4..MV_LONG_BITS).rev() {
            x += i32::from(bd.read(p[MV_LONG + i])) << i;
        }
        // Bit 3 is implied when nothing above it is set, since a long
        // vector is at least 8.
        if x & 0xfff0 == 0 || bd.read(p[MV_LONG + 3]) {
            x += 8;
        }
    } else {
        x = i32::from(bd.read_tree(&SMALL_MV_TREE, &p[MV_SHORT..MV_SHORT + 7]));
    }
    if x != 0 && bd.read(p[MV_SIGN]) {
        x = -x;
    }
    (x * 2) as i16
}

/// Read a vector coded as an offset from `base`.
fn read_mv(bd: &mut BoolDecoder, probs: &[[u8; MV_PROB_COUNT]; 2], base: Mv) -> Mv {
    let row = read_mv_component(bd, &probs[0]);
    let col = read_mv_component(bd, &probs[1]);
    Mv { row: row.wrapping_add(base.row), col: col.wrapping_add(base.col) }
}

/// The intra subblock mode a whole-macroblock mode stands for, as context.
fn implied_bmode(mode: u8) -> u8 {
    match mode {
        V_PRED => B_VE_PRED,
        H_PRED => B_HE_PRED,
        TM_PRED => B_TM_PRED,
        _ => B_DC_PRED,
    }
}

/// Parses the mode partition one macroblock at a time. `mbs` has a border
/// entry before each row and a border row on top: macroblock (col, row) is
/// at `(row + 1) * stride + col + 1`.
pub struct ModeParser<'h> {
    pub header: &'h FrameHeader,
    pub probs: &'h Probs,
    pub mb_cols: usize,
    pub mb_rows: usize,
    pub stride: usize,
}

impl ModeParser<'_> {
    pub fn parse(&self, bd: &mut BoolDecoder, mbs: &mut [MbInfo], mb_col: usize, mb_row: usize) {
        let h = self.header;
        let idx = (mb_row + 1) * self.stride + mb_col + 1;
        // Split so the neighbours (all earlier in the array) can be read
        // while the current entry is written.
        let (before, rest) = mbs.split_at_mut(idx);
        let mb = &mut rest[0];
        let left = &before[idx - 1];
        let above = &before[idx - self.stride];
        let above_left = &before[idx - self.stride - 1];

        if h.update_segment_map {
            let p = &h.segment_tree_probs;
            mb.segment = if bd.read(p[0]) { 2 + u8::from(bd.read(p[2])) } else { u8::from(bd.read(p[1])) };
        } else if h.key_frame {
            mb.segment = 0;
        }
        mb.skip = h.skip_enabled && bd.read(h.prob_skip_false);
        mb.need_clamp = false;

        if h.key_frame {
            mb.ref_frame = INTRA_FRAME;
            mb.mv = Mv::ZERO;
            mb.mode = bd.read_tree(&KF_YMODE_TREE, &KF_YMODE_PROBS);
            if mb.mode == B_PRED {
                for i in 0..16 {
                    let a = if i < 4 { above.bmodes[i + 12] } else { mb.bmodes[i - 4] };
                    let l = if i & 3 == 0 { left.bmodes[i + 3] } else { mb.bmodes[i - 1] };
                    mb.bmodes[i] = bd.read_tree(&BMODE_TREE, &KF_BMODE_PROBS[usize::from(a)][usize::from(l)]);
                }
            } else {
                mb.bmodes = [implied_bmode(mb.mode); 16];
            }
            mb.uv_mode = bd.read_tree(&UV_MODE_TREE, &KF_UV_MODE_PROBS);
        } else if bd.read(h.prob_intra) {
            self.parse_inter(bd, mb, left, above, above_left, mb_col, mb_row);
        } else {
            mb.ref_frame = INTRA_FRAME;
            mb.mv = Mv::ZERO;
            mb.mode = bd.read_tree(&YMODE_TREE, &self.probs.ymode);
            if mb.mode == B_PRED {
                for m in &mut mb.bmodes {
                    *m = bd.read_tree(&BMODE_TREE, &BMODE_PROBS);
                }
            }
            mb.uv_mode = bd.read_tree(&UV_MODE_TREE, &self.probs.uv_mode);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn parse_inter(
        &self,
        bd: &mut BoolDecoder,
        mb: &mut MbInfo,
        left: &MbInfo,
        above: &MbInfo,
        above_left: &MbInfo,
        mb_col: usize,
        mb_row: usize,
    ) {
        let h = self.header;
        mb.ref_frame = if bd.read(h.prob_last) { 2 + u8::from(bd.read(h.prob_golden)) } else { 1 };
        mb.uv_mode = DC_PRED;

        // Survey the neighbours' vectors: near[0] stays zero, near[1..] are
        // the distinct non-zero vectors found, cnt[i] the weight of near[i]
        // (above and left count 2, above-left 1).
        let mut near = [Mv::ZERO; 4];
        let mut cnt = [0usize; 4];
        let mut found = 0;
        let my_bias = h.sign_bias[usize::from(mb.ref_frame)];
        for (nb, weight) in [(above, 2), (left, 2), (above_left, 1)] {
            if nb.ref_frame == INTRA_FRAME {
                continue;
            }
            if nb.mv == Mv::ZERO {
                cnt[0] += weight;
                continue;
            }
            // A vector into a frame on the other side in time points the
            // opposite way.
            let mut mv = nb.mv;
            if h.sign_bias[usize::from(nb.ref_frame)] != my_bias {
                mv = Mv { row: mv.row.wrapping_neg(), col: mv.col.wrapping_neg() };
            }
            if found == 0 || mv != near[found] {
                found += 1;
                near[found] = mv;
            }
            cnt[found] += weight;
        }

        let bounds = MvBounds::new(mb_col, mb_row, self.mb_cols, self.mb_rows).widened(MV_MARGIN, MV_MARGIN);

        if !bd.read(MODE_CONTEXTS[cnt[0]][0]) {
            mb.mode = ZEROMV;
            mb.mv = Mv::ZERO;
            return;
        }
        // With three distinct vectors, above-left's joins the first if equal.
        if cnt[3] > 0 && near[found] == near[1] {
            cnt[1] += 1;
        }
        if cnt[2] > cnt[1] {
            cnt.swap(1, 2);
            near.swap(1, 2);
        }
        if !bd.read(MODE_CONTEXTS[cnt[1]][1]) {
            mb.mode = NEARESTMV;
            mb.mv = bounds.clamp(near[1]);
            return;
        }
        if !bd.read(MODE_CONTEXTS[cnt[2]][2]) {
            mb.mode = NEARMV;
            mb.mv = bounds.clamp(near[2]);
            return;
        }
        // New vectors are coded relative to the most popular neighbour.
        let best = bounds.clamp(if cnt[1] >= cnt[0] { near[1] } else { near[0] });
        let split_census = (usize::from(above.mode == SPLITMV) + usize::from(left.mode == SPLITMV)) * 2
            + usize::from(above_left.mode == SPLITMV);
        if !bd.read(MODE_CONTEXTS[split_census][3]) {
            mb.mode = NEWMV;
            mb.mv = read_mv(bd, &self.probs.mv, best);
            mb.need_clamp = !bounds.contains(mb.mv);
            return;
        }

        mb.mode = SPLITMV;
        let split = if !bd.read(MB_SPLIT_PROBS[0]) {
            3
        } else if !bd.read(MB_SPLIT_PROBS[1]) {
            2
        } else {
            usize::from(bd.read(MB_SPLIT_PROBS[2]))
        };
        mb.split = split as u8;
        let layout = &MB_SPLITS[split];
        for part in 0..MB_SPLIT_COUNT[split] {
            // Context: the vectors left of and above the part's first block.
            let k = usize::from(MB_SPLIT_FIRST[split][usize::from(part)]);
            let left_mv = if k & 3 != 0 {
                mb.bmvs[k - 1]
            } else if left.mode == SPLITMV {
                left.bmvs[k + 3]
            } else {
                left.mv
            };
            let above_mv = if k >= 4 {
                mb.bmvs[k - 4]
            } else if above.mode == SPLITMV {
                above.bmvs[k + 12]
            } else {
                above.mv
            };
            let ctx = usize::from(above_mv == Mv::ZERO) << 2
                | usize::from(left_mv == Mv::ZERO) << 1
                | usize::from(left_mv == above_mv);
            let p = &SUB_MV_REF_PROBS[ctx];
            let mv = if !bd.read(p[0]) {
                left_mv
            } else if !bd.read(p[1]) {
                above_mv
            } else if !bd.read(p[2]) {
                Mv::ZERO
            } else {
                read_mv(bd, &self.probs.mv, best)
            };
            mb.need_clamp |= !bounds.contains(mv);
            for (b, &owner) in mb.bmvs.iter_mut().zip(layout) {
                if owner == part {
                    *b = mv;
                }
            }
        }
        mb.mv = mb.bmvs[15];
    }
}
