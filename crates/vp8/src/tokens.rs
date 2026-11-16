//! Coefficient token decoding and dequantization (RFC 6386 sections 13
//! and 14.1).

use crate::bool_decoder::BoolDecoder;
use crate::header::Dequant;
use crate::modes::MbInfo;
use crate::tables::*;

/// "Has coefficients" flags of the blocks along one macroblock edge, the
/// context for the first token of the neighbouring blocks: 4 luma, 2 U,
/// 2 V, then the Y2 block.
pub type EdgeContext = [u8; 9];

/// Index of the Y2 block in the coefficient buffer.
pub const Y2: usize = 24;

/// The dequantized coefficients of one macroblock: 16 luma blocks, 4 U,
/// 4 V and the Y2 block, each in raster (not zigzag) order.
pub struct Coeffs {
    pub blocks: [[i16; 16]; 25],
    /// Position after the last decoded token of each block; 0 means the
    /// block is empty and at most 1 means only DC can be non-zero.
    pub eob: [u8; 25],
}

impl Coeffs {
    pub fn new() -> Coeffs {
        Coeffs { blocks: [[0; 16]; 25], eob: [0; 25] }
    }
}

type BlockProbs = [[[u8; 11]; 3]; 8];

/// Decode the tokens of one block starting at coefficient `first`, storing
/// each coefficient times its dequantization factor (`dq` is DC, AC; the
/// product wraps to 16 bits as libvpx's does). Returns the position after
/// the last token, or 0 if there is none.
#[inline(always)]
fn decode_block(
    bd: &mut BoolDecoder,
    probs: &BlockProbs,
    ctx: usize,
    first: usize,
    dq: [i16; 2],
    out: &mut [i16; 16],
) -> usize {
    let mut n = first;
    let mut p = &probs[n][ctx];
    if !bd.read(p[0]) {
        return 0;
    }
    loop {
        n += 1;
        let band = &probs[usize::from(COEFF_BANDS[n])];
        if !bd.read(p[1]) {
            // A zero: no end-of-block check follows it.
            p = &band[0];
        } else {
            let v: i32;
            if !bd.read(p[2]) {
                v = 1;
                p = &band[1];
            } else {
                if !bd.read(p[3]) {
                    v = if !bd.read(p[4]) { 2 } else { 3 + i32::from(bd.read(p[5])) };
                } else if !bd.read(p[6]) {
                    if !bd.read(p[7]) {
                        v = 5 + i32::from(bd.read(159));
                    } else {
                        let hi = i32::from(bd.read(165));
                        v = 7 + 2 * hi + i32::from(bd.read(145));
                    }
                } else {
                    let bit1 = usize::from(bd.read(p[8]));
                    let bit0 = usize::from(bd.read(p[9 + bit1]));
                    let cat = 2 * bit1 + bit0;
                    let mut extra = 0;
                    for &prob in CAT_PROBS[cat] {
                        extra = extra * 2 + i32::from(bd.read(prob));
                    }
                    v = extra + 3 + (8 << cat);
                }
                p = &band[2];
            }
            let v = if bd.read_bit() { -v } else { v } as i16;
            out[usize::from(ZIGZAG[n - 1]) & 15] = v.wrapping_mul(dq[usize::from(n > 1)]);
            if n == 16 || !bd.read(p[0]) {
                return n;
            }
        }
        if n == 16 {
            return 16;
        }
    }
}

/// Decode all coefficients of a macroblock that is not skipped. Returns
/// whether any block had tokens.
pub fn decode_mb(
    bd: &mut BoolDecoder,
    probs: &[BlockProbs; 4],
    mb: &MbInfo,
    dq: &Dequant,
    above: &mut EdgeContext,
    left: &mut EdgeContext,
    coeffs: &mut Coeffs,
) -> bool {
    // Work on a local copy of the decoder so its state stays in registers
    // across the stores into the coefficient buffer.
    let stream = bd;
    let mut local = stream.clone();
    let bd = &mut local;
    let mut any = 0;
    let (luma_probs, first) = if mb.has_y2() {
        let n = decode_block(bd, &probs[1], usize::from(above[8] + left[8]), 0, dq.y2, &mut coeffs.blocks[Y2]);
        above[8] = u8::from(n > 0);
        left[8] = above[8];
        coeffs.eob[Y2] = n as u8;
        any |= n;
        // The luma blocks' own DC position is not coded; their factor for
        // it is unused.
        (&probs[0], 1)
    } else {
        (&probs[3], 0)
    };
    for i in 0..16 {
        let (a, l) = (&mut above[i & 3], &mut left[i >> 2]);
        let n = decode_block(bd, luma_probs, usize::from(*a + *l), first, dq.y1, &mut coeffs.blocks[i]);
        *a = u8::from(n > 0);
        *l = *a;
        coeffs.eob[i] = n as u8;
        any |= n;
    }
    for i in 16..24 {
        // U blocks 16..20 then V blocks 20..24, two by two each.
        let plane = 4 + ((i - 16) >> 2) * 2;
        let (a, l) = (&mut above[plane + (i & 1)], &mut left[plane + ((i >> 1) & 1)]);
        let n = decode_block(bd, &probs[2], usize::from(*a + *l), 0, dq.uv, &mut coeffs.blocks[i]);
        *a = u8::from(n > 0);
        *l = *a;
        coeffs.eob[i] = n as u8;
        any |= n;
    }
    *stream = local;
    any != 0
}

/// Update the contexts for a macroblock flagged as having no coefficients.
pub fn skip_mb(mb: &MbInfo, above: &mut EdgeContext, left: &mut EdgeContext) {
    above[..8].fill(0);
    left[..8].fill(0);
    // Without a Y2 block of its own, the macroblock leaves the Y2 context
    // for the next one that has one.
    if mb.has_y2() {
        above[8] = 0;
        left[8] = 0;
    }
}
