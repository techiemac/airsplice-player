//! Inter prediction: building a macroblock from a reference frame
//! (RFC 6386 section 18).

use crate::dsp;
use crate::frame::{FrameBuf, Plane};
use crate::modes::{MbInfo, Mv, MvBounds};
use crate::tables::SPLITMV;

/// Which interpolation the stream's version selects.
#[derive(Clone, Copy)]
pub struct Interp {
    /// Bilinear instead of six-tap (versions 1 to 3).
    pub bilinear: bool,
    /// Chroma vectors are rounded to whole pixels (version 3).
    pub full_pixel: bool,
}

/// Predict a `w` x `h` block at (`x`, `y`) of `dst` from the same place in
/// `src` displaced by `mv` (eighths of a pixel in this plane).
///
/// Invariant: after clamping, `mv` keeps the block within 19 pixels (luma;
/// about 10 for chroma) of the macroblock-aligned picture, so with the
/// filter taps every read lands inside the plane's border.
#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn predict_block(src: &Plane, dst: &mut Plane, x: usize, y: usize, w: usize, h: usize, mv: Mv, interp: Interp) {
    let stride = src.stride;
    let src_at = (src.at(x, y) as isize + isize::from(mv.row >> 3) * stride as isize + isize::from(mv.col >> 3)) as usize;
    let dst_at = dst.at(x, y);
    let (mx, my) = ((mv.col & 7) as u8, (mv.row & 7) as u8);
    if mx | my == 0 {
        match w {
            16 => copy_block::<16>(&src.data[src_at..], &mut dst.data[dst_at..], stride, h),
            8 => copy_block::<8>(&src.data[src_at..], &mut dst.data[dst_at..], stride, h),
            _ => copy_block::<4>(&src.data[src_at..], &mut dst.data[dst_at..], stride, h),
        }
    } else if interp.bilinear {
        dsp::bilinear(&src.data, src_at, stride, &mut dst.data, dst_at, stride, w, h, mx, my);
    } else {
        dsp::sixtap(&src.data, src_at, stride, &mut dst.data, dst_at, stride, w, h, mx, my);
    }
}

/// Copy `h` rows of `W` pixels. The width is a constant so each row is one
/// load and store instead of a call to memcpy.
#[inline(always)]
fn copy_block<const W: usize>(src: &[u8], dst: &mut [u8], stride: usize, h: usize) {
    for (s, d) in src.chunks(stride).zip(dst.chunks_mut(stride)).take(h) {
        if let (Some(s), Some(d)) = (s.first_chunk::<W>(), d.first_chunk_mut::<W>()) {
            *d = *s;
        }
    }
}

/// Pull a luma vector that points far outside the frame back to where the
/// prediction is the same (pure border), dropping its fraction.
fn clamp_luma(mv: Mv, b: &MvBounds) -> Mv {
    let clamp = |v: i16, lo: i32, hi: i32| {
        let v = i32::from(v);
        (if v < lo - (19 << 3) {
            lo - (16 << 3)
        } else if v > hi + (18 << 3) {
            hi + (16 << 3)
        } else {
            v
        }) as i16
    };
    Mv { row: clamp(mv.row, b.top, b.bottom), col: clamp(mv.col, b.left, b.right) }
}

/// The same for a chroma vector, which is in units half the size.
fn clamp_chroma(mv: Mv, b: &MvBounds) -> Mv {
    let clamp = |v: i16, lo: i32, hi: i32| {
        let mut v = i32::from(v);
        if 2 * v < lo - (19 << 3) {
            v = (lo - (16 << 3)) >> 1;
        }
        if 2 * v > hi + (18 << 3) {
            v = (hi + (16 << 3)) >> 1;
        }
        v as i16
    };
    Mv { row: clamp(mv.row, b.top, b.bottom), col: clamp(mv.col, b.left, b.right) }
}

/// Predict all three planes of an inter macroblock. `bounds` are the
/// macroblock's distances to the frame edges, without margin.
pub fn predict_mb(
    reference: &FrameBuf,
    cur: &mut FrameBuf,
    mb_col: usize,
    mb_row: usize,
    mb: &MbInfo,
    bounds: &MvBounds,
    interp: Interp,
) {
    let (x, y) = (mb_col * 16, mb_row * 16);
    let (cx, cy) = (mb_col * 8, mb_row * 8);
    let mask: i32 = if interp.full_pixel { !7 } else { !0 };

    if mb.mode != SPLITMV {
        let mv = if mb.need_clamp { clamp_luma(mb.mv, bounds) } else { mb.mv };
        predict_block(&reference.y, &mut cur.y, x, y, 16, 16, mv, interp);
        // Chroma planes are half size, so the same number is a vector of
        // twice the precision; halve it rounding away from zero.
        let half = |v: i16| {
            let v = i32::from(v.wrapping_add(if v < 0 { -1 } else { 1 }));
            ((v / 2) & mask) as i16
        };
        let cmv = clamp_chroma(Mv { row: half(mv.row), col: half(mv.col) }, bounds);
        predict_block(&reference.u, &mut cur.u, cx, cy, 8, 8, cmv, interp);
        predict_block(&reference.v, &mut cur.v, cx, cy, 8, 8, cmv, interp);
        return;
    }

    let luma = |i: usize| if mb.need_clamp { clamp_luma(mb.bmvs[i], bounds) } else { mb.bmvs[i] };
    if mb.split < 3 {
        // Every layout but 4x4 is uniform over each 8x8 quadrant.
        for i in [0, 2, 8, 10] {
            predict_block(&reference.y, &mut cur.y, x + (i & 3) * 4, y + (i >> 2) * 4, 8, 8, luma(i), interp);
        }
    } else {
        for i in 0..16 {
            predict_block(&reference.y, &mut cur.y, x + (i & 3) * 4, y + (i >> 2) * 4, 4, 4, luma(i), interp);
        }
    }
    // Each 4x4 chroma block uses the average of the four luma vectors
    // (unclamped) that cover it.
    for i in 0..4 {
        let (bx, by) = (i & 1, i >> 1);
        let first = by * 8 + bx * 2;
        let avg = |get: fn(&Mv) -> i16| {
            let sum: i32 = [first, first + 1, first + 4, first + 5].iter().map(|&k| i32::from(get(&mb.bmvs[k]))).sum();
            (((sum + if sum < 0 { -4 } else { 4 }) / 8) & mask) as i16
        };
        let mut cmv = Mv { row: avg(|m| m.row), col: avg(|m| m.col) };
        if mb.need_clamp {
            cmv = clamp_chroma(cmv, bounds);
        }
        predict_block(&reference.u, &mut cur.u, cx + bx * 4, cy + by * 4, 4, 4, cmv, interp);
        predict_block(&reference.v, &mut cur.v, cx + bx * 4, cy + by * 4, 4, 4, cmv, interp);
    }
}
