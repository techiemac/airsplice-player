//! Intra prediction (RFC 6386 section 12).

use crate::tables::*;

/// Predict an `N` x `N` block (16 for luma, 8 for chroma) at `dst[at..]`
/// from the row above, the column to the left and the pixel above-left.
/// Outside the frame those hold 127 (above) and 129 (left), which is what
/// every mode but DC uses there; DC averages only the edges that exist.
#[allow(clippy::too_many_arguments)]
pub fn predict_mb<const N: usize>(
    dst: &mut [u8],
    at: usize,
    stride: usize,
    mode: u8,
    above: &[u8; N],
    left: &[u8; N],
    top_left: u8,
    have_above: bool,
    have_left: bool,
) {
    let mut rows = dst[at..].chunks_mut(stride).take(N);
    let mut next_row = || -> &mut [u8; N] { rows.next().and_then(|r| r.first_chunk_mut()).expect("block inside the plane") };
    match mode {
        V_PRED => {
            for _ in 0..N {
                *next_row() = *above;
            }
        }
        H_PRED => {
            for &l in left {
                *next_row() = [l; N];
            }
        }
        TM_PRED => {
            for &l in left {
                let base = i32::from(l) - i32::from(top_left);
                for (d, &a) in next_row().iter_mut().zip(above) {
                    *d = (base + i32::from(a)).clamp(0, 255) as u8;
                }
            }
        }
        _ => {
            let sum = |edge: &[u8; N]| edge.iter().map(|&p| u32::from(p)).sum::<u32>();
            let shift = N.trailing_zeros();
            let n = N as u32;
            let dc = match (have_above, have_left) {
                (true, true) => (sum(above) + sum(left) + n) >> (shift + 1),
                (true, false) => (sum(above) + n / 2) >> shift,
                (false, true) => (sum(left) + n / 2) >> shift,
                (false, false) => 128,
            };
            for _ in 0..N {
                *next_row() = [dc as u8; N];
            }
        }
    }
}

/// Predict one 4x4 luma subblock. `above` is the 4 pixels above the block
/// followed by the 4 above and to its right, `left` the 4 to its left.
/// Returns the block's rows.
pub fn predict_subblock(mode: u8, above: &[u8; 8], left: &[u8; 4], top_left: u8) -> [[u8; 4]; 4] {
    let avg2 = |a: u8, b: u8| ((u32::from(a) + u32::from(b) + 1) >> 1) as u8;
    let avg3 = |a: u8, b: u8, c: u8| ((u32::from(a) + 2 * u32::from(b) + u32::from(c) + 2) >> 2) as u8;
    let [a, b, c, d, e, f, g, h] = *above;
    let [i, j, k, l] = *left;
    let x = top_left;
    match mode {
        B_DC_PRED => {
            let sum: u32 = above[..4].iter().chain(left).map(|&p| u32::from(p)).sum();
            [[((sum + 4) >> 3) as u8; 4]; 4]
        }
        B_TM_PRED => {
            let mut out = [[0; 4]; 4];
            for (row, &lp) in out.iter_mut().zip(left) {
                for (o, &ap) in row.iter_mut().zip(above) {
                    *o = (i32::from(lp) + i32::from(ap) - i32::from(x)).clamp(0, 255) as u8;
                }
            }
            out
        }
        B_VE_PRED => [[avg3(x, a, b), avg3(a, b, c), avg3(b, c, d), avg3(c, d, e)]; 4],
        B_HE_PRED => [[avg3(x, i, j); 4], [avg3(i, j, k); 4], [avg3(j, k, l); 4], [avg3(k, l, l); 4]],
        B_LD_PRED => {
            // Each diagonal running down and to the left shares one value.
            let v = [avg3(a, b, c), avg3(b, c, d), avg3(c, d, e), avg3(d, e, f), avg3(e, f, g), avg3(f, g, h), avg3(g, h, h)];
            std::array::from_fn(|r| std::array::from_fn(|col| v[r + col]))
        }
        B_RD_PRED => {
            // Diagonals running down and to the right, from the bottom-left
            // corner of the edge to its top-right.
            let v = [avg3(j, k, l), avg3(i, j, k), avg3(x, i, j), avg3(a, x, i), avg3(b, a, x), avg3(c, b, a), avg3(d, c, b)];
            std::array::from_fn(|r| std::array::from_fn(|col| v[3 + col - r]))
        }
        B_VR_PRED => {
            let (r0, r1) = ([avg2(x, a), avg2(a, b), avg2(b, c), avg2(c, d)], [avg3(i, x, a), avg3(x, a, b), avg3(a, b, c), avg3(b, c, d)]);
            [r0, r1, [avg3(j, i, x), r0[0], r0[1], r0[2]], [avg3(k, j, i), r1[0], r1[1], r1[2]]]
        }
        B_VL_PRED => {
            let (r0, r1) = ([avg2(a, b), avg2(b, c), avg2(c, d), avg2(d, e)], [avg3(a, b, c), avg3(b, c, d), avg3(c, d, e), avg3(d, e, f)]);
            [r0, r1, [r0[1], r0[2], r0[3], avg3(e, f, g)], [r1[1], r1[2], r1[3], avg3(f, g, h)]]
        }
        B_HD_PRED => {
            let r0 = [avg2(i, x), avg3(i, x, a), avg3(x, a, b), avg3(a, b, c)];
            let r1 = [avg2(j, i), avg3(j, i, x), r0[0], r0[1]];
            let r2 = [avg2(k, j), avg3(k, j, i), r1[0], r1[1]];
            [r0, r1, r2, [avg2(l, k), avg3(l, k, j), r2[0], r2[1]]]
        }
        _ => {
            // B_HU_PRED
            let r1 = [avg2(j, k), avg3(j, k, l), avg2(k, l), avg3(k, l, l)];
            [[avg2(i, j), avg3(i, j, k), r1[0], r1[1]], r1, [r1[2], r1[3], l, l], [l; 4]]
        }
    }
}
