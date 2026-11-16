//! Pixel kernels: inverse transforms, loop filters and sub-pixel motion
//! compensation, as RFC 6386 and libvpx's C reference define them. Every
//! function must match libvpx bit for bit.
//!
//! Plane addressing: a plane is a `&[u8]` / `&mut [u8]` with a row `stride`;
//! a position is a byte offset into it. Callers guarantee every pixel a
//! kernel reads or writes is inside the slice (frame buffers have borders).

use std::array;
use std::ops::Range;

/// Inverse DCT of one 4x4 block of dequantized coefficients (row-major,
/// natural order, not zigzag), added to the prediction at `dst[at..]` with
/// clamping to 0..=255. libvpx: `vp8_short_idct4x4llm_c`.
pub fn idct_add(coeffs: &[i16; 16], dst: &mut [u8], at: usize, stride: usize) {
    // The vertical pass comes first, as in libvpx. Each pass runs on four
    // lanes at once: the columns for the vertical pass, then the rows.
    let rows: [[i32; 4]; 4] = array::from_fn(|r| array::from_fn(|c| i32::from(coeffs[4 * r + c])));
    let t = idct_pass(rows);
    // libvpx keeps both passes' results in `short`, so each is truncated to
    // 16 bits; a stream with oversized coefficients depends on that wrap.
    let cols: [[i32; 4]; 4] = array::from_fn(|c| array::from_fn(|r| i32::from(t[r][c] as i16)));
    let o = idct_pass(cols);
    for r in 0..4 {
        let out = &mut dst[at + r * stride..][..4];
        for (c, px) in out.iter_mut().enumerate() {
            let residual = ((o[c][r] + 4) >> 3) as i16;
            *px = (i32::from(*px) + i32::from(residual)).clamp(0, 255) as u8;
        }
    }
}

/// sqrt(2) * cos(pi / 8) - 1 and sqrt(2) * sin(pi / 8), in 16.16 fixed point.
const COSPI8_SQRT2_MINUS1: i32 = 20091;
const SINPI8_SQRT2: i32 = 35468;

/// One pass of the inverse DCT over four lanes: `i[k][n]` is input `k` of
/// the 4-point transform in lane `n`, and likewise for the outputs.
#[inline(always)]
fn idct_pass(i: [[i32; 4]; 4]) -> [[i32; 4]; 4] {
    let mut o = [[0i32; 4]; 4];
    for n in 0..4 {
        let (i0, i1, i2, i3) = (i[0][n], i[1][n], i[2][n], i[3][n]);
        let a = i0 + i2;
        let b = i0 - i2;
        let c = ((i1 * SINPI8_SQRT2) >> 16) - (i3 + ((i3 * COSPI8_SQRT2_MINUS1) >> 16));
        let d = (i1 + ((i1 * COSPI8_SQRT2_MINUS1) >> 16)) + ((i3 * SINPI8_SQRT2) >> 16);
        o[0][n] = a + d;
        o[1][n] = b + c;
        o[2][n] = b - c;
        o[3][n] = a - d;
    }
    o
}

/// The DC-only shortcut: adds `(dc + 4) >> 3` to every pixel of the 4x4
/// block. libvpx: `vp8_dc_only_idct_add_c`.
pub fn idct_dc_add(dc: i16, dst: &mut [u8], at: usize, stride: usize) {
    let add = (i32::from(dc) + 4) >> 3;
    // One of these is zero, so the pair of saturating byte operations is
    // the clamped wide addition.
    // (Measured: gathering the block into one 16-byte vector first is
    // slower.)
    let (up, down) = (add.clamp(0, 255) as u8, (-add).clamp(0, 255) as u8);
    for r in 0..4 {
        for px in &mut dst[at + r * stride..][..4] {
            *px = px.saturating_add(up).saturating_sub(down);
        }
    }
}

/// Inverse Walsh-Hadamard transform of the Y2 block. Output `i` is the DC
/// coefficient of luma subblock `i` (raster order). libvpx:
/// `vp8_short_inv_walsh4x4_c`.
pub fn iwht(input: &[i16; 16]) -> [i16; 16] {
    // As in the IDCT, libvpx stores the first pass in `short`.
    let mut tmp = [0i16; 16];
    for i in 0..4 {
        let (i0, i1, i2, i3) = (i32::from(input[i]), i32::from(input[4 + i]), i32::from(input[8 + i]), i32::from(input[12 + i]));
        let a = i0 + i3;
        let b = i1 + i2;
        let c = i1 - i2;
        let d = i0 - i3;
        tmp[i] = (a + b) as i16;
        tmp[4 + i] = (c + d) as i16;
        tmp[8 + i] = (a - b) as i16;
        tmp[12 + i] = (d - c) as i16;
    }
    let mut out = [0i16; 16];
    for (o, t) in out.as_chunks_mut::<4>().0.iter_mut().zip(tmp.as_chunks::<4>().0) {
        let (i0, i1, i2, i3) = (i32::from(t[0]), i32::from(t[1]), i32::from(t[2]), i32::from(t[3]));
        let a = i0 + i3;
        let b = i1 + i2;
        let c = i1 - i2;
        let d = i0 - i3;
        o[0] = ((a + b + 3) >> 3) as i16;
        o[1] = ((c + d + 3) >> 3) as i16;
        o[2] = ((a - b + 3) >> 3) as i16;
        o[3] = ((d - c + 3) >> 3) as i16;
    }
    out
}

/// Loop filter strength for one edge.
#[derive(Clone, Copy, Debug)]
pub struct EdgeLimits {
    /// The edge limit: `((filter_level + 2) * 2) + interior_limit` for
    /// macroblock edges, `(filter_level * 2) + interior_limit` for subblock
    /// edges.
    pub edge: u8,
    /// The interior limit.
    pub interior: u8,
    /// The high edge variance threshold.
    pub hev_threshold: u8,
}

// Loop filters. Each filters `count` positions along one edge. `at` is the
// first pixel after the edge (q0) at the first position; the pixels across the
// edge are at `at - 4*step .. at + 3*step` (p3..q3), and successive positions
// are `along` bytes apart. For a vertical edge (filtering horizontally),
// `step = 1, along = stride`; for a horizontal edge, `step = stride, along = 1`.
// The positions of one call must not share pixels (they never do in those two
// layouts): they are filtered together, not one after another.

/// Normal filter across a macroblock edge. libvpx:
/// `vp8_mbloop_filter_horizontal_edge_c` / `..._vertical_edge_c`.
pub fn normal_mb_edge(buf: &mut [u8], at: usize, step: usize, along: usize, count: usize, l: EdgeLimits) {
    filter_edge(buf, at, step, along, count, MbEdge(l));
}

/// Normal filter across a subblock (inner) edge. libvpx:
/// `vp8_loop_filter_horizontal_edge_c` / `..._vertical_edge_c`.
pub fn normal_subblock_edge(buf: &mut [u8], at: usize, step: usize, along: usize, count: usize, l: EdgeLimits) {
    filter_edge(buf, at, step, along, count, SubblockEdge(l));
}

/// Simple filter (luma only), across any edge. `edge_limit` as in
/// [`EdgeLimits::edge`]. libvpx: `vp8_loop_filter_simple_horizontal_edge_c`
/// / `..._vertical_edge_c`.
pub fn simple_edge(buf: &mut [u8], at: usize, step: usize, along: usize, count: usize, edge_limit: u8) {
    filter_edge(buf, at, step, along, count, SimpleEdge(edge_limit));
}

// The filters are written without branches over a block of positions, so
// that the compiler turns each into vector code: an edge's pixels are
// gathered into one array per tap, filtered lane by lane, and the taps a
// filter changes are written back. A position the filter mask rejects gets
// a zero adjustment, which leaves its pixels as they were.

/// `px[k][i]` is tap `k` (p3, p2, p1, p0, q0, q1, q2, q3) at position `i`.
type Lanes<const N: usize> = [[u8; N]; 8];

trait EdgeFilter: Copy {
    /// The taps the filter looks at, and the ones it changes.
    const READS: Range<usize>;
    const WRITES: Range<usize>;

    fn filter<const N: usize>(self, px: &mut Lanes<N>);
}

#[inline(always)]
fn filter_edge<F: EdgeFilter>(buf: &mut [u8], at: usize, step: usize, along: usize, count: usize, f: F) {
    debug_assert!(along >= 8 * step || step >= count * along, "positions share pixels");
    let mut done = 0;
    while done < count {
        let at = at + done * along;
        // Luma edges are 16 long and chroma edges 8. Passing those lengths
        // as constants matters: the copies in and out then compile to plain
        // loads and stores instead of calls to memcpy.
        done += match count - done {
            16.. => filter_lanes::<F, 16, 128>(buf, at, step, along, 16, f),
            // (Measured: across a vertical edge, 8 positions go faster as
            // half of a 16-lane block than as an 8-lane one.)
            8 if step == 1 => filter_lanes::<F, 16, 128>(buf, at, step, along, 8, f),
            8 => filter_lanes::<F, 8, 64>(buf, at, step, along, 8, f),
            left => filter_partial(buf, at, step, along, left, f),
        };
    }
}

/// Any other length is padded with lanes that are filtered and thrown away.
#[inline(never)]
fn filter_partial<F: EdgeFilter>(buf: &mut [u8], at: usize, step: usize, along: usize, n: usize, f: F) -> usize {
    filter_lanes::<F, 16, 128>(buf, at, step, along, n, f)
}

/// Filters the first `n` of `N` lanes and returns `n`. `LEN` is `8 * N`,
/// the size of all eight taps of every lane.
#[inline(always)]
fn filter_lanes<F: EdgeFilter, const N: usize, const LEN: usize>(buf: &mut [u8], at: usize, step: usize, along: usize, n: usize, f: F) -> usize {
    const { assert!(LEN == 8 * N && N.is_power_of_two()) };
    let n = n.min(N);
    let mut px = [[0u8; N]; 8];
    // `at + k * step >= 4 * step` for every tap read, as `at` is at least
    // `(4 - READS.start) * step` into the plane.
    if along == 1 {
        for k in F::READS {
            px[k][..n].copy_from_slice(&buf[at + k * step - 4 * step..][..n]);
        }
    } else if step == 1 {
        // The eight taps of a position are adjacent in the picture, so
        // gathering them is a transposition: lay the positions end to end,
        // then shuffle them into one run per tap. All eight are read (and
        // written back below), whichever the filter uses.
        let mut flat = [0u8; LEN];
        for (i, position) in flat.as_chunks_mut::<8>().0.iter_mut().enumerate().take(n) {
            *position = *pixels(buf, at + i * along - 4);
        }
        for _ in 0..N.trailing_zeros() {
            flat = interleave(&flat);
        }
        for (tap, run) in px.iter_mut().zip(flat.as_chunks().0) {
            *tap = *run;
        }
    } else {
        for i in 0..n {
            for k in F::READS {
                px[k][i] = buf[at + i * along + k * step - 4 * step];
            }
        }
    }
    f.filter(&mut px);
    if along == 1 {
        for k in F::WRITES {
            buf[at + k * step - 4 * step..][..n].copy_from_slice(&px[k][..n]);
        }
    } else if step == 1 {
        // Shuffling the eight runs together transposes them back.
        let mut flat = [0u8; LEN];
        for (run, tap) in flat.as_chunks_mut().0.iter_mut().zip(&px) {
            *run = *tap;
        }
        for _ in 0..3 {
            flat = interleave(&flat);
        }
        for (i, position) in flat.as_chunks::<8>().0.iter().enumerate().take(n) {
            *pixels_mut(buf, at + i * along - 4) = *position;
        }
    } else {
        for i in 0..n {
            for k in F::WRITES {
                buf[at + i * along + k * step - 4 * step] = px[k][i];
            }
        }
    }
    n
}

/// One step of a perfect shuffle: the first half of `src` interleaved with
/// the second. Applied k times to 2^k equal runs laid end to end, it
/// leaves the first elements of all the runs in order, then the second
/// elements, and so on, which transposes them; and the compiler has vector
/// instructions for exactly this loop.
#[inline(always)]
fn interleave<const LEN: usize>(src: &[u8; LEN]) -> [u8; LEN] {
    let mut out = [0u8; LEN];
    for i in 0..LEN / 2 {
        out[2 * i] = src[i];
        out[2 * i + 1] = src[LEN / 2 + i];
    }
    out
}

// The filters work on pixels as signed chars (the value XOR 0x80), with
// every intermediate saturated to that range, as libvpx's
// `vp8_signed_char_clamp` does. Masks are -1 or 0.

#[inline(always)]
fn signed(p: u8) -> i8 {
    (p ^ 0x80) as i8
}

#[inline(always)]
fn unsigned(v: i8) -> u8 {
    v as u8 ^ 0x80
}

/// Whether `|p0 - q0| * 2 + |p1 - q1| / 2 <= limit`.
#[inline(always)]
fn edge_mask(limit: u8, p1: u8, p0: u8, q0: u8, q1: u8) -> i8 {
    // Rearranged to `|p0 - q0| <= (limit - |p1 - q1| / 2) / 2` so that
    // nothing exceeds a byte and 16 lanes fit one vector.
    let half = p1.abs_diff(q1) >> 1;
    let room = limit.wrapping_sub(half) >> 1;
    -i8::from((half <= limit) & (p0.abs_diff(q0) <= room))
}

/// Whether every difference between neighbours on each side of the edge is
/// within the interior limit.
#[inline(always)]
fn interior_mask<const N: usize>(limit: u8, px: &Lanes<N>, i: usize) -> i8 {
    let d = |a: usize, b: usize| px[a][i].abs_diff(px[b][i]);
    let most = d(0, 1).max(d(1, 2)).max(d(2, 3)).max(d(4, 5)).max(d(5, 6)).max(d(6, 7));
    -i8::from(most <= limit)
}

#[inline(always)]
fn hev_mask(threshold: u8, p1: u8, p0: u8, q0: u8, q1: u8) -> i8 {
    -i8::from(p1.abs_diff(p0).max(q1.abs_diff(q0)) > threshold)
}

/// `clamp(outer + 3 * (q0 - p0))`, the filter value every variant starts
/// from. Three saturating additions of the saturated difference give the
/// same result as the wide sum: they all push the same way, so the first
/// one to saturate is where the wide sum leaves the range too.
#[inline(always)]
fn filter_value(outer: i8, ps0: i8, qs0: i8) -> i8 {
    let d = qs0.saturating_sub(ps0);
    outer.saturating_add(d).saturating_add(d).saturating_add(d)
}

/// Moves p0 and q0 towards each other by about `w / 8`. Rounding one side
/// with +4 and the other with +3 keeps their sum when the step is odd.
/// Returns the new p0 and q0 and the amount q0 moved.
#[inline(always)]
fn adjust_inner(w: i8, ps0: i8, qs0: i8) -> (i8, i8, i8) {
    let f1 = w.saturating_add(4) >> 3;
    let f2 = w.saturating_add(3) >> 3;
    (ps0.saturating_add(f2), qs0.saturating_sub(f1), f1)
}

#[derive(Clone, Copy)]
struct SimpleEdge(u8);

impl EdgeFilter for SimpleEdge {
    const READS: Range<usize> = 2..6;
    const WRITES: Range<usize> = 3..5;

    #[inline(always)]
    fn filter<const N: usize>(self, px: &mut Lanes<N>) {
        for i in 0..N {
            let (p1, p0, q0, q1) = (px[2][i], px[3][i], px[4][i], px[5][i]);
            let mask = edge_mask(self.0, p1, p0, q0, q1);
            let (ps0, qs0) = (signed(p0), signed(q0));
            let w = filter_value(signed(p1).saturating_sub(signed(q1)), ps0, qs0) & mask;
            let (ps0, qs0, _) = adjust_inner(w, ps0, qs0);
            px[3][i] = unsigned(ps0);
            px[4][i] = unsigned(qs0);
        }
    }
}

#[derive(Clone, Copy)]
struct SubblockEdge(EdgeLimits);

impl EdgeFilter for SubblockEdge {
    const READS: Range<usize> = 0..8;
    const WRITES: Range<usize> = 2..6;

    #[inline(always)]
    fn filter<const N: usize>(self, px: &mut Lanes<N>) {
        let l = self.0;
        for i in 0..N {
            let (p1, p0, q0, q1) = (px[2][i], px[3][i], px[4][i], px[5][i]);
            let mask = edge_mask(l.edge, p1, p0, q0, q1) & interior_mask(l.interior, px, i);
            let hev = hev_mask(l.hev_threshold, p1, p0, q0, q1);
            let (ps1, ps0, qs0, qs1) = (signed(p1), signed(p0), signed(q0), signed(q1));
            // The outer taps join the filter only on high edge variance.
            let w = filter_value(ps1.saturating_sub(qs1) & hev, ps0, qs0) & mask;
            let (ps0, qs0, f1) = adjust_inner(w, ps0, qs0);
            // Without high variance the outer pixels move by half as much.
            let a = ((f1 + 1) >> 1) & !hev;
            px[2][i] = unsigned(ps1.saturating_add(a));
            px[3][i] = unsigned(ps0);
            px[4][i] = unsigned(qs0);
            px[5][i] = unsigned(qs1.saturating_sub(a));
        }
    }
}

#[derive(Clone, Copy)]
struct MbEdge(EdgeLimits);

impl EdgeFilter for MbEdge {
    const READS: Range<usize> = 0..8;
    const WRITES: Range<usize> = 1..7;

    #[inline(always)]
    fn filter<const N: usize>(self, px: &mut Lanes<N>) {
        let l = self.0;
        for i in 0..N {
            let (p2, p1, p0, q0, q1, q2) = (px[1][i], px[2][i], px[3][i], px[4][i], px[5][i], px[6][i]);
            let mask = edge_mask(l.edge, p1, p0, q0, q1) & interior_mask(l.interior, px, i);
            let hev = hev_mask(l.hev_threshold, p1, p0, q0, q1);
            let (ps2, ps1, ps0) = (signed(p2), signed(p1), signed(p0));
            let (qs0, qs1, qs2) = (signed(q0), signed(q1), signed(q2));
            let w = filter_value(ps1.saturating_sub(qs1), ps0, qs0) & mask;
            // High variance: only the two pixels at the edge move.
            let (ps0, qs0, _) = adjust_inner(w & hev, ps0, qs0);
            // Otherwise three pixels on each side move, by roughly 3/7, 2/7
            // and 1/7 of the difference across the edge. Each step is at
            // most 27 in magnitude, so it needs no clamp of its own.
            let w = i16::from(w & !hev);
            let a = ((63 + w * 27) >> 7) as i8;
            px[3][i] = unsigned(ps0.saturating_add(a));
            px[4][i] = unsigned(qs0.saturating_sub(a));
            let a = ((63 + w * 18) >> 7) as i8;
            px[2][i] = unsigned(ps1.saturating_add(a));
            px[5][i] = unsigned(qs1.saturating_sub(a));
            let a = ((63 + w * 9) >> 7) as i8;
            px[1][i] = unsigned(ps2.saturating_add(a));
            px[6][i] = unsigned(qs2.saturating_sub(a));
        }
    }
}

// Sub-pixel prediction. Predicts a `w` x `h` block whose top-left full-pixel
// source position is `src[src_at]`, offset by `mx`, `my` eighths of a pixel
// (0..=7; luma callers pass quarter-pel motion times two, as libvpx does).
// The six-tap filter reads 2 pixels before and 3 after the block in each
// filtered direction. `w` and `h` are 4, 8 or 16 (chroma split blocks use 4).
//
// libvpx always filters in both directions, horizontally and then
// vertically, with a pass-through filter for a zero offset. A pass-through
// pass returns its input exactly, so it is skipped here, and with it the
// reads of pixels that only that pass would have touched.

/// Six-tap interpolation (version 0). libvpx: `vp8_sixtap_predict*_c`,
/// including its rule that a zero offset in one direction is a pass-through
/// in that direction.
#[allow(clippy::too_many_arguments)]
pub fn sixtap(src: &[u8], src_at: usize, src_stride: usize, dst: &mut [u8], dst_at: usize, dst_stride: usize, w: usize, h: usize, mx: u8, my: u8) {
    let f = SixTap(&SIXTAP[usize::from(mx)], &SIXTAP[usize::from(my)]);
    predict(src, src_at, src_stride, dst, dst_at, dst_stride, w, h, mx != 0, my != 0, f);
}

/// Bilinear interpolation (versions 1 and 2). libvpx: `vp8_bilinear_predict*_c`.
#[allow(clippy::too_many_arguments)]
pub fn bilinear(src: &[u8], src_at: usize, src_stride: usize, dst: &mut [u8], dst_at: usize, dst_stride: usize, w: usize, h: usize, mx: u8, my: u8) {
    assert!(mx < 8 && my < 8);
    let f = Bilinear(u16::from(mx), u16::from(my));
    predict(src, src_at, src_stride, dst, dst_at, dst_stride, w, h, mx != 0, my != 0, f);
}

/// A separable interpolation filter over `TAPS` pixels, of which `BEFORE`
/// precede the pixel being predicted.
trait Interpolate: Copy {
    const TAPS: usize;
    const BEFORE: usize;

    /// One output row from `TAPS` input rows (for the horizontal pass, the
    /// same row at successive one-pixel shifts). Unused entries of `taps`
    /// repeat the last one.
    fn row<const W: usize>(self, vertical: bool, taps: &[&[u8; W]; 6], out: &mut [u8; W]);
}

#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn predict<F: Interpolate>(src: &[u8], src_at: usize, src_stride: usize, dst: &mut [u8], dst_at: usize, dst_stride: usize, w: usize, h: usize, x: bool, y: bool, f: F) {
    // A fixed width per call lets the row loops compile to straight vector
    // code with no bounds checks inside.
    match w {
        4 => predict_w::<F, 4>(src, src_at, src_stride, dst, dst_at, dst_stride, h, x, y, f),
        8 => predict_w::<F, 8>(src, src_at, src_stride, dst, dst_at, dst_stride, h, x, y, f),
        16 => predict_w::<F, 16>(src, src_at, src_stride, dst, dst_at, dst_stride, h, x, y, f),
        _ => panic!("prediction block width {w} is not 4, 8 or 16"),
    }
}

/// The `W` pixels at `buf[at..]`.
#[inline(always)]
fn pixels<const W: usize>(buf: &[u8], at: usize) -> &[u8; W] {
    buf[at..].first_chunk().expect("row inside the plane")
}

#[inline(always)]
fn pixels_mut<const W: usize>(buf: &mut [u8], at: usize) -> &mut [u8; W] {
    buf[at..].first_chunk_mut().expect("row inside the plane")
}

#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn predict_w<F: Interpolate, const W: usize>(src: &[u8], src_at: usize, src_stride: usize, dst: &mut [u8], dst_at: usize, dst_stride: usize, h: usize, x: bool, y: bool, f: F) {
    assert!(h <= 16, "prediction block height {h} is more than 16");
    let last = F::TAPS - 1;
    // The horizontal pass of the source row starting at `at`. Rows are
    // written in place, never returned: a 4 or 8-pixel array returned by
    // value is packed into an integer, which defeats vectorization.
    let across = |at: usize, out: &mut [u8; W]| {
        let row = &src[at - F::BEFORE..][..W + last];
        f.row(false, &array::from_fn(|k| pixels(row, k.min(last))), out);
    };
    let top = src_at - if y { F::BEFORE * src_stride } else { 0 };
    match (x, y) {
        (false, false) => {
            for r in 0..h {
                *pixels_mut::<W>(dst, dst_at + r * dst_stride) = *pixels(src, src_at + r * src_stride);
            }
        }
        (true, false) => {
            for r in 0..h {
                across(src_at + r * src_stride, pixels_mut(dst, dst_at + r * dst_stride));
            }
        }
        (false, true) => {
            for r in 0..h {
                let taps: [&[u8; W]; 6] = array::from_fn(|k| pixels(src, top + (r + k.min(last)) * src_stride));
                f.row(true, &taps, pixels_mut(dst, dst_at + r * dst_stride));
            }
        }
        (true, true) => {
            // The vertical pass needs TAPS - 1 rows beyond the block's own.
            let mut tmp = [[0u8; W]; 21];
            for (r, t) in tmp[..h + last].iter_mut().enumerate() {
                across(top + r * src_stride, t);
            }
            for r in 0..h {
                let taps: [&[u8; W]; 6] = array::from_fn(|k| &tmp[r + k.min(last)]);
                f.row(true, &taps, pixels_mut(dst, dst_at + r * dst_stride));
            }
        }
    }
}

/// Six-tap filters by eighth-pel offset; each sums to 128. Offset 0 passes
/// the pixel through, and the odd offsets are really four-tap.
const SIXTAP: [[i16; 6]; 8] = [
    [0, 0, 128, 0, 0, 0],
    [0, -6, 123, 12, -1, 0],
    [2, -11, 108, 36, -8, 1],
    [0, -9, 93, 50, -6, 0],
    [3, -16, 77, 77, -16, 3],
    [0, -6, 50, 93, -9, 0],
    [1, -8, 36, 108, -11, 2],
    [0, -1, 12, 123, -6, 0],
];

/// The horizontal and vertical six-tap filters.
#[derive(Clone, Copy)]
struct SixTap<'a>(&'a [i16; 6], &'a [i16; 6]);

impl Interpolate for SixTap<'_> {
    const TAPS: usize = 6;
    const BEFORE: usize = 2;

    #[inline(always)]
    fn row<const W: usize>(self, vertical: bool, taps: &[&[u8; W]; 6], out: &mut [u8; W]) {
        let f = if vertical { self.1 } else { self.0 };
        // The sum over unsigned pixels can reach 255 * 160, too much for 16
        // bits. Taking 128 off each pixel bounds it by 128 * 192, and
        // because the taps sum to 128 the bias comes back out as exactly
        // 128 after the shift.
        let t = |k: usize, x: usize| i16::from((taps[k][x] ^ 0x80) as i8) * f[k];
        for (x, out) in out.iter_mut().enumerate() {
            let sum = t(0, x) + t(1, x) + t(2, x) + t(3, x) + t(4, x) + t(5, x);
            *out = (((sum + 64) >> 7) + 128).clamp(0, 255) as u8;
        }
    }
}

/// The horizontal and vertical offsets in eighths. The taps are
/// `128 - 16 * offset` and `16 * offset`, rounded with `+ 64 >> 7`; dividing
/// everything by 16 gives the same result from `8 - offset` and `offset`
/// with `+ 4 >> 3`.
#[derive(Clone, Copy)]
struct Bilinear(u16, u16);

impl Interpolate for Bilinear {
    const TAPS: usize = 2;
    const BEFORE: usize = 0;

    #[inline(always)]
    fn row<const W: usize>(self, vertical: bool, taps: &[&[u8; W]; 6], out: &mut [u8; W]) {
        let m = if vertical { self.1 } else { self.0 };
        for (x, out) in out.iter_mut().enumerate() {
            *out = ((u16::from(taps[0][x]) * (8 - m) + u16::from(taps[1][x]) * m + 4) >> 3) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kernels written plainly from RFC 6386, one position or pixel at
    /// a time in wide integers, for the real ones to be checked against.
    mod reference {
        use super::EdgeLimits;

        /// Inverse DCT of one 4x4 block of dequantized coefficients (row-major,
        /// natural order, not zigzag), added to the prediction at `dst[at..]` with
        /// clamping to 0..=255. libvpx: `vp8_short_idct4x4llm_c`.
        pub fn idct_add(coeffs: &[i16; 16], dst: &mut [u8], at: usize, stride: usize) {
            // libvpx keeps both passes' results in `short`, so each is truncated to
            // 16 bits; a stream with oversized coefficients depends on that wrap.
            let mut tmp = [0i16; 16];
            for i in 0..4 {
                let col = idct_1d(coeffs[i], coeffs[4 + i], coeffs[8 + i], coeffs[12 + i]);
                for (r, v) in col.into_iter().enumerate() {
                    tmp[4 * r + i] = v as i16;
                }
            }
            for r in 0..4 {
                let row = idct_1d(tmp[4 * r], tmp[4 * r + 1], tmp[4 * r + 2], tmp[4 * r + 3]);
                let out = &mut dst[at + r * stride..][..4];
                for (px, v) in out.iter_mut().zip(row) {
                    let residual = ((v + 4) >> 3) as i16;
                    *px = (i32::from(*px) + i32::from(residual)).clamp(0, 255) as u8;
                }
            }
        }

        /// sqrt(2) * cos(pi / 8) - 1 and sqrt(2) * sin(pi / 8), in 16.16 fixed point.
        const COSPI8_SQRT2_MINUS1: i32 = 20091;
        const SINPI8_SQRT2: i32 = 35468;

        /// One 4-point pass of the inverse DCT, vertical first and then horizontal.
        #[inline(always)]
        fn idct_1d(i0: i16, i1: i16, i2: i16, i3: i16) -> [i32; 4] {
            let (i0, i1, i2, i3) = (i32::from(i0), i32::from(i1), i32::from(i2), i32::from(i3));
            let a = i0 + i2;
            let b = i0 - i2;
            let c = ((i1 * SINPI8_SQRT2) >> 16) - (i3 + ((i3 * COSPI8_SQRT2_MINUS1) >> 16));
            let d = (i1 + ((i1 * COSPI8_SQRT2_MINUS1) >> 16)) + ((i3 * SINPI8_SQRT2) >> 16);
            [a + d, b + c, b - c, a - d]
        }

        /// Normal filter across a macroblock edge. libvpx:
        /// `vp8_mbloop_filter_horizontal_edge_c` / `..._vertical_edge_c`.
        pub fn normal_mb_edge(buf: &mut [u8], at: usize, step: usize, along: usize, count: usize, l: EdgeLimits) {
            for i in 0..count {
                let q0 = at + i * along;
                let px = |k: usize| buf[q0 + k * step - 4 * step];
                let (p3, p2, p1, p0, q0v, q1, q2, q3) = (px(0), px(1), px(2), px(3), px(4), px(5), px(6), px(7));
                if !filter_mask(l, [p3, p2, p1, p0, q0v, q1, q2, q3]) {
                    continue;
                }
                let hev = high_edge_variance(l.hev_threshold, p1, p0, q0v, q1);
                let (ps2, ps1, ps0) = (signed(p2), signed(p1), signed(p0));
                let (qs0, qs1, qs2) = (signed(q0v), signed(q1), signed(q2));
                let w = clamp8(clamp8(ps1 - qs1) + 3 * (qs0 - ps0));
                let (ps0, qs0) = if hev {
                    // High variance: only the two pixels at the edge move.
                    let f1 = clamp8(w + 4) >> 3;
                    let f2 = clamp8(w + 3) >> 3;
                    (clamp8(ps0 + f2), clamp8(qs0 - f1))
                } else {
                    (ps0, qs0)
                };
                let w = if hev { 0 } else { w };
                // Roughly 3/7, 2/7 and 1/7 of the difference across the edge.
                let a = clamp8((63 + w * 27) >> 7);
                buf[q0] = unsigned(clamp8(qs0 - a));
                buf[q0 - step] = unsigned(clamp8(ps0 + a));
                let a = clamp8((63 + w * 18) >> 7);
                buf[q0 + step] = unsigned(clamp8(qs1 - a));
                buf[q0 - 2 * step] = unsigned(clamp8(ps1 + a));
                let a = clamp8((63 + w * 9) >> 7);
                buf[q0 + 2 * step] = unsigned(clamp8(qs2 - a));
                buf[q0 - 3 * step] = unsigned(clamp8(ps2 + a));
            }
        }

        // The filters work on pixels as signed chars (the value XOR 0x80), with
        // every intermediate saturated to that range, as libvpx's
        // `vp8_signed_char_clamp` does.

        #[inline(always)]
        fn signed(p: u8) -> i32 {
            i32::from((p ^ 0x80) as i8)
        }

        #[inline(always)]
        fn unsigned(v: i32) -> u8 {
            (v as u8) ^ 0x80
        }

        #[inline(always)]
        fn clamp8(v: i32) -> i32 {
            v.clamp(-128, 127)
        }

        /// Whether a position is filtered at all: the step across the edge is
        /// within the edge limit and every neighbouring difference on each side is
        /// within the interior limit. `p` is p3, p2, p1, p0, q0, q1, q2, q3.
        #[inline(always)]
        fn filter_mask(l: EdgeLimits, p: [u8; 8]) -> bool {
            let i = l.interior;
            p[0].abs_diff(p[1]) <= i
                && p[1].abs_diff(p[2]) <= i
                && p[2].abs_diff(p[3]) <= i
                && p[5].abs_diff(p[4]) <= i
                && p[6].abs_diff(p[5]) <= i
                && p[7].abs_diff(p[6]) <= i
                && simple_mask(l.edge, p[2], p[3], p[4], p[5])
        }

        #[inline(always)]
        fn simple_mask(edge_limit: u8, p1: u8, p0: u8, q0: u8, q1: u8) -> bool {
            u32::from(p0.abs_diff(q0)) * 2 + u32::from(p1.abs_diff(q1)) / 2 <= u32::from(edge_limit)
        }

        #[inline(always)]
        fn high_edge_variance(threshold: u8, p1: u8, p0: u8, q0: u8, q1: u8) -> bool {
            p1.abs_diff(p0) > threshold || q1.abs_diff(q0) > threshold
        }

        /// Normal filter across a subblock (inner) edge. libvpx:
        /// `vp8_loop_filter_horizontal_edge_c` / `..._vertical_edge_c`.
        pub fn normal_subblock_edge(buf: &mut [u8], at: usize, step: usize, along: usize, count: usize, l: EdgeLimits) {
            for i in 0..count {
                let q0 = at + i * along;
                let px = |k: usize| buf[q0 + k * step - 4 * step];
                let (p3, p2, p1, p0, q0v, q1, q2, q3) = (px(0), px(1), px(2), px(3), px(4), px(5), px(6), px(7));
                if !filter_mask(l, [p3, p2, p1, p0, q0v, q1, q2, q3]) {
                    continue;
                }
                let hev = high_edge_variance(l.hev_threshold, p1, p0, q0v, q1);
                let (ps1, ps0, qs0, qs1) = (signed(p1), signed(p0), signed(q0v), signed(q1));
                // The outer taps join the filter only on high edge variance.
                let outer = if hev { clamp8(ps1 - qs1) } else { 0 };
                let w = clamp8(outer + 3 * (qs0 - ps0));
                let f1 = clamp8(w + 4) >> 3;
                let f2 = clamp8(w + 3) >> 3;
                buf[q0] = unsigned(clamp8(qs0 - f1));
                buf[q0 - step] = unsigned(clamp8(ps0 + f2));
                // Without high variance the outer pixels move by half as much.
                let a = if hev { 0 } else { (f1 + 1) >> 1 };
                buf[q0 + step] = unsigned(clamp8(qs1 - a));
                buf[q0 - 2 * step] = unsigned(clamp8(ps1 + a));
            }
        }

        /// Simple filter (luma only), across any edge. `edge_limit` as in
        /// [`EdgeLimits::edge`]. libvpx: `vp8_loop_filter_simple_horizontal_edge_c`
        /// / `..._vertical_edge_c`.
        pub fn simple_edge(buf: &mut [u8], at: usize, step: usize, along: usize, count: usize, edge_limit: u8) {
            for i in 0..count {
                let q0 = at + i * along;
                let (p1, p0, q0v, q1) = (buf[q0 - 2 * step], buf[q0 - step], buf[q0], buf[q0 + step]);
                if !simple_mask(edge_limit, p1, p0, q0v, q1) {
                    continue;
                }
                let (ps1, ps0, qs0, qs1) = (signed(p1), signed(p0), signed(q0v), signed(q1));
                let w = clamp8(clamp8(ps1 - qs1) + 3 * (qs0 - ps0));
                // Rounding one side with +4 and the other with +3 keeps the pair's
                // sum unchanged when the adjustment is odd.
                let f1 = clamp8(w + 4) >> 3;
                let f2 = clamp8(w + 3) >> 3;
                buf[q0] = unsigned(clamp8(qs0 - f1));
                buf[q0 - step] = unsigned(clamp8(ps0 + f2));
            }
        }

        /// Six-tap interpolation (version 0). libvpx: `vp8_sixtap_predict*_c`,
        /// including its rule that a zero offset in one direction is a pass-through
        /// in that direction.
        #[allow(clippy::too_many_arguments)]
        pub fn sixtap(src: &[u8], src_at: usize, src_stride: usize, dst: &mut [u8], dst_at: usize, dst_stride: usize, w: usize, h: usize, mx: u8, my: u8) {
            assert!(w <= 16 && h <= 16);
            let fx = &SIXTAP[usize::from(mx)];
            let fy = &SIXTAP[usize::from(my)];
            // First pass: horizontal, over the five extra rows the vertical taps need.
            let mut tmp = [[0u8; 16]; 21];
            for (r, row) in tmp[..h + 5].iter_mut().enumerate() {
                let s = &src[src_at + r * src_stride - 2 * src_stride - 2..][..w + 5];
                for (x, out) in row[..w].iter_mut().enumerate() {
                    let sum: i32 = (0..6).map(|k| i32::from(s[x + k]) * fx[k]).sum();
                    *out = ((sum + 64) >> 7).clamp(0, 255) as u8;
                }
            }
            for y in 0..h {
                let out = &mut dst[dst_at + y * dst_stride..][..w];
                for (x, out) in out.iter_mut().enumerate() {
                    let sum: i32 = (0..6).map(|k| i32::from(tmp[y + k][x]) * fy[k]).sum();
                    *out = ((sum + 64) >> 7).clamp(0, 255) as u8;
                }
            }
        }

        /// Six-tap filters by eighth-pel offset; each sums to 128. Offset 0 passes
        /// the pixel through, and the odd offsets are really four-tap.
        const SIXTAP: [[i32; 6]; 8] = [
            [0, 0, 128, 0, 0, 0],
            [0, -6, 123, 12, -1, 0],
            [2, -11, 108, 36, -8, 1],
            [0, -9, 93, 50, -6, 0],
            [3, -16, 77, 77, -16, 3],
            [0, -6, 50, 93, -9, 0],
            [1, -8, 36, 108, -11, 2],
            [0, -1, 12, 123, -6, 0],
        ];

        /// Bilinear interpolation (versions 1 and 2). libvpx: `vp8_bilinear_predict*_c`.
        #[allow(clippy::too_many_arguments)]
        pub fn bilinear(src: &[u8], src_at: usize, src_stride: usize, dst: &mut [u8], dst_at: usize, dst_stride: usize, w: usize, h: usize, mx: u8, my: u8) {
            assert!(w <= 16 && h <= 16);
            let (fx0, fx1) = (128 - 16 * u32::from(mx), 16 * u32::from(mx));
            let (fy0, fy1) = (128 - 16 * u32::from(my), 16 * u32::from(my));
            // A zero offset has a zero second tap. libvpx still reads that pixel;
            // skipping the read gives the same result without needing it in bounds.
            let mut tmp = [[0u8; 16]; 17];
            let rows = if my == 0 { h } else { h + 1 };
            for (r, row) in tmp[..rows].iter_mut().enumerate() {
                let o = src_at + r * src_stride;
                for (x, out) in row[..w].iter_mut().enumerate() {
                    let next = if mx == 0 { 0 } else { u32::from(src[o + x + 1]) };
                    *out = ((u32::from(src[o + x]) * fx0 + next * fx1 + 64) >> 7) as u8;
                }
            }
            for y in 0..h {
                let out = &mut dst[dst_at + y * dst_stride..][..w];
                for (x, out) in out.iter_mut().enumerate() {
                    *out = ((u32::from(tmp[y][x]) * fy0 + u32::from(tmp[y + 1][x]) * fy1 + 64) >> 7) as u8;
                }
            }
        }
    }

    /// xorshift64: enough for test inputs, and trivially mirrored in C.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        /// Pixels of one of four kinds: noise, only 0 and 255, a smooth
        /// area around a random level, a smooth area near black or white.
        /// The smooth kinds are the ones the loop filters actually modify.
        fn fill(&mut self, buf: &mut [u8]) {
            let r0 = self.next();
            let kind = r0 % 4;
            let mut base = ((r0 >> 8) & 255) as i32;
            let mut amp = 1i32 << ((r0 >> 16) % 6);
            if kind == 3 {
                base = if (r0 >> 8) & 1 != 0 { 250 } else { 5 };
                amp = 8;
            }
            for b in buf {
                let r = self.next() >> 32;
                *b = match kind {
                    0 => r as u8,
                    1 => if r & 1 != 0 { 255 } else { 0 },
                    _ => (base + (r % (2 * amp as u64 + 1)) as i32 - amp).clamp(0, 255) as u8,
                };
            }
        }

        /// A coefficient: small, plausible, anything a `short` holds, or
        /// mostly zero with the occasional arbitrary value.
        fn coef(&mut self, mode: usize) -> i16 {
            let r = self.next();
            match mode {
                0 => (((r >> 20) % 512) as i32 - 256) as i16,
                1 => (((r >> 20) % 4096) as i32 - 2048) as i16,
                2 => (r >> 20) as i16,
                _ => if (r >> 40).is_multiple_of(8) { (r >> 20) as i16 } else { 0 },
            }
        }
    }

    /// FNV-1a.
    struct Digest(u64);

    impl Digest {
        fn new() -> Self {
            Digest(0xcbf2_9ce4_8422_2325)
        }

        fn add(&mut self, bytes: &[u8]) {
            for &b in bytes {
                self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
            }
        }
    }

    // The `libvpx_*` tests replay a fixed input stream through the kernels
    // and compare a digest of everything they wrote (including the untouched
    // surroundings) with the digest of libvpx 1.16.0's C functions over the
    // same stream. The digests came from a C program linked against
    // libvpx.a that mirrors each loop below call for call, using
    // `vp8_short_idct4x4llm_c`, `vp8_dc_only_idct_add_c`,
    // `vp8_short_inv_walsh4x4_c`, `vp8_loop_filter_{mbv,bv,mbh,bh}_c`,
    // `vp8_loop_filter_simple_{vertical,horizontal}_edge_c`,
    // `vp8_loop_filter_{bvs,bhs}_c`, `vp8_sixtap_predict*_c` and
    // `vp8_bilinear_predict*_c`.

    #[test]
    fn libvpx_idct() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut digest = Digest::new();
        for i in 0..40000 {
            let mut c = [0i16; 16];
            for v in &mut c {
                *v = rng.coef(i % 4);
            }
            let mut p = [0u8; 64];
            rng.fill(&mut p);
            idct_add(&c, &mut p, 18, 8);
            digest.add(&p);
        }
        assert_eq!(digest.0, 0x2a27_50b6_c74f_7fb6);
    }

    #[test]
    fn libvpx_idct_dc() {
        let mut rng = Rng(0xD1B5_4A32_D192_ED03);
        let mut digest = Digest::new();
        for i in 0..40000 {
            let dc = rng.coef(if i % 2 != 0 { 2 } else { 1 });
            let mut p = [0u8; 64];
            rng.fill(&mut p);
            idct_dc_add(dc, &mut p, 18, 8);
            digest.add(&p);
        }
        assert_eq!(digest.0, 0x07db_73e3_0e76_d8f4);
    }

    #[test]
    fn libvpx_iwht() {
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        let mut digest = Digest::new();
        for i in 0..40000 {
            let mut c = [0i16; 16];
            for v in &mut c {
                *v = rng.coef(i % 4);
            }
            for v in iwht(&c) {
                digest.add(&v.to_le_bytes());
            }
        }
        assert_eq!(digest.0, 0x8863_eb85_93dd_4b07);
    }

    /// A limit byte: usually within `mask`, one time in four any value.
    fn limit(r: u64, select: u32, shift: u32, mask: u8) -> u8 {
        let v = (r >> shift) as u8;
        if (r >> select) & 3 == 0 { v } else { v & mask }
    }

    #[test]
    fn libvpx_normal_loop_filter() {
        let mut rng = Rng(0xA076_1D64_78BD_642F);
        let mut digest = Digest::new();
        let mut changed = 0usize;
        for _ in 0..40000 {
            let (mut y, mut u, mut v) = ([0u8; 1024], [0u8; 256], [0u8; 256]);
            rng.fill(&mut y);
            rng.fill(&mut u);
            rng.fill(&mut v);
            let before = y;
            let r = rng.next();
            let interior = limit(r, 0, 8, 63);
            let hev_threshold = limit(r, 6, 32, 3);
            let mb = EdgeLimits { edge: limit(r, 2, 16, 127), interior, hev_threshold };
            let sb = EdgeLimits { edge: limit(r, 4, 24, 127), interior, hev_threshold };
            // One macroblock at (8, 8) in luma and (4, 4) in chroma, filtered
            // in the decoder's order: left edge, inner vertical edges, top
            // edge, inner horizontal edges.
            let (yo, co) = (8 * 32 + 8, 4 * 16 + 4);
            normal_mb_edge(&mut y, yo, 1, 32, 16, mb);
            normal_mb_edge(&mut u, co, 1, 16, 8, mb);
            normal_mb_edge(&mut v, co, 1, 16, 8, mb);
            for k in [4, 8, 12] {
                normal_subblock_edge(&mut y, yo + k, 1, 32, 16, sb);
            }
            normal_subblock_edge(&mut u, co + 4, 1, 16, 8, sb);
            normal_subblock_edge(&mut v, co + 4, 1, 16, 8, sb);
            normal_mb_edge(&mut y, yo, 32, 1, 16, mb);
            normal_mb_edge(&mut u, co, 16, 1, 8, mb);
            normal_mb_edge(&mut v, co, 16, 1, 8, mb);
            for k in [4, 8, 12] {
                normal_subblock_edge(&mut y, yo + k * 32, 32, 1, 16, sb);
            }
            normal_subblock_edge(&mut u, co + 4 * 16, 16, 1, 8, sb);
            normal_subblock_edge(&mut v, co + 4 * 16, 16, 1, 8, sb);
            digest.add(&y);
            digest.add(&u);
            digest.add(&v);
            changed += y.iter().zip(&before).filter(|(a, b)| a != b).count();
        }
        // The stream must exercise the filters, not only their masks.
        assert!(changed > 1_000_000, "only {changed} luma pixels filtered");
        assert_eq!(digest.0, 0xc537_8c79_e569_a201);
    }

    #[test]
    fn libvpx_simple_loop_filter() {
        let mut rng = Rng(0xE703_7ED1_A0B4_28DB);
        let mut digest = Digest::new();
        let mut changed = 0usize;
        for _ in 0..40000 {
            let mut y = [0u8; 1024];
            rng.fill(&mut y);
            let before = y;
            let r = rng.next();
            let mb = limit(r, 0, 16, 127);
            let sb = limit(r, 2, 24, 127);
            let yo = 8 * 32 + 8;
            simple_edge(&mut y, yo, 1, 32, 16, mb);
            for k in [4, 8, 12] {
                simple_edge(&mut y, yo + k, 1, 32, 16, sb);
            }
            simple_edge(&mut y, yo, 32, 1, 16, mb);
            for k in [4, 8, 12] {
                simple_edge(&mut y, yo + k * 32, 32, 1, 16, sb);
            }
            digest.add(&y);
            changed += y.iter().zip(&before).filter(|(a, b)| a != b).count();
        }
        assert!(changed > 1_000_000, "only {changed} luma pixels filtered");
        assert_eq!(digest.0, 0x4eb8_c8a7_28ff_635c);
    }

    type Predict = fn(&[u8], usize, usize, &mut [u8], usize, usize, usize, usize, u8, u8);

    /// libvpx's four block sizes, every offset pair, 16 source blocks each.
    fn predict_digest(seed: u64, predict: Predict, skip_copy: bool) -> u64 {
        let mut rng = Rng(seed);
        let mut digest = Digest::new();
        for (w, h) in [(4, 4), (8, 4), (8, 8), (16, 16)] {
            for mx in 0..8 {
                for my in 0..8 {
                    for _ in 0..16 {
                        let mut src = [0u8; 1024];
                        rng.fill(&mut src);
                        if skip_copy && mx == 0 && my == 0 {
                            continue;
                        }
                        let mut dst = [0xAAu8; 576];
                        predict(&src, 8 * 32 + 8, 32, &mut dst, 4 * 24 + 4, 24, w, h, mx, my);
                        digest.add(&dst);
                    }
                }
            }
        }
        digest.0
    }

    #[test]
    fn libvpx_sixtap() {
        assert_eq!(predict_digest(0x5899_65CC_7537_4CC3, sixtap, false), 0xf0d8_8d64_ba25_20b5);
    }

    #[test]
    fn libvpx_bilinear() {
        // libvpx asserts that its bilinear functions are never asked to copy.
        assert_eq!(predict_digest(0x8EBC_6AF0_9C88_C6E3, bilinear, true), 0x448d_9582_cc25_42f8);
    }

    // Known answers that follow from the RFC's formulas by hand.

    #[test]
    fn dc_only_idct_equals_the_shortcut() {
        let mut rng = Rng(1);
        for dc in i16::MIN..=i16::MAX {
            let mut a = [0u8; 16];
            rng.fill(&mut a);
            let mut b = a;
            let mut c = [0i16; 16];
            c[0] = dc;
            idct_add(&c, &mut a, 0, 4);
            idct_dc_add(dc, &mut b, 0, 4);
            assert_eq!(a, b, "dc {dc}");
        }
    }

    #[test]
    fn idct_dc_add_rounds_and_clamps() {
        let mut p = [100u8; 16];
        idct_dc_add(11, &mut p, 0, 4); // (11 + 4) >> 3 = 1
        assert_eq!(p, [101; 16]);
        idct_dc_add(-5, &mut p, 0, 4); // (-5 + 4) >> 3 = -1
        assert_eq!(p, [100; 16]);
        idct_dc_add(2000, &mut p, 0, 4);
        assert_eq!(p, [255; 16]);
        idct_dc_add(-3000, &mut p, 0, 4);
        assert_eq!(p, [0; 16]);
    }

    #[test]
    fn idct_writes_only_its_block() {
        let mut p = [7u8; 64];
        idct_add(&[800; 16], &mut p, 18, 8);
        for (i, &v) in p.iter().enumerate() {
            let inside = (2..6).contains(&(i / 8)) && (2..6).contains(&(i % 8));
            assert!(inside || v == 7, "pixel {i} changed");
        }
    }

    #[test]
    fn iwht_spreads_dc() {
        for dc in i16::MIN..=i16::MAX {
            let mut c = [0i16; 16];
            c[0] = dc;
            assert_eq!(iwht(&c), [((i32::from(dc) + 3) >> 3) as i16; 16]);
        }
    }

    #[test]
    fn iwht_of_a_constant_is_dc_only() {
        // Sixteen equal inputs sum to 16 * v in output 0 and cancel elsewhere.
        let mut expect = [0i16; 16];
        expect[0] = (16 * 100 + 3) >> 3;
        assert_eq!(iwht(&[100; 16]), expect);
    }

    const ALL_LIMITS: EdgeLimits = EdgeLimits { edge: 255, interior: 255, hev_threshold: 0 };

    #[test]
    fn flat_regions_pass_through_every_filter() {
        for level in 0..=255u8 {
            for (step, along) in [(1, 16), (16, 1)] {
                let flat = [level; 256];
                let at = 8 * 16 + 8;
                let mut p = flat;
                normal_mb_edge(&mut p, at - 4 * along, step, along, 8, ALL_LIMITS);
                assert_eq!(p, flat);
                normal_subblock_edge(&mut p, at - 4 * along, step, along, 8, ALL_LIMITS);
                assert_eq!(p, flat);
                simple_edge(&mut p, at - 4 * along, step, along, 8, 255);
                assert_eq!(p, flat);
            }
        }
    }

    /// One position across a vertical edge: p3..q3 in a row.
    fn row(p: [u8; 8]) -> [u8; 8] {
        p
    }

    #[test]
    fn simple_filter_known_answers() {
        // p1 = q1, so the filter value is 3 * (q0 - p0) = 24; q0 drops by
        // (24 + 4) >> 3 = 3 and p0 rises by (24 + 3) >> 3 = 3.
        let mut p = row([0, 0, 100, 100, 108, 100, 0, 0]);
        simple_edge(&mut p, 4, 1, 8, 1, 16);
        assert_eq!(p, [0, 0, 100, 103, 105, 100, 0, 0]);
        // |p0 - q0| * 2 + |p1 - q1| / 2 = 16 exceeds a limit of 15.
        let mut p = row([0, 0, 100, 100, 108, 100, 0, 0]);
        simple_edge(&mut p, 4, 1, 8, 1, 15);
        assert_eq!(p, [0, 0, 100, 100, 108, 100, 0, 0]);
        // An odd adjustment: 3 * 3 = 9, so q0 drops by 13 >> 3 = 1 and p0
        // rises by 12 >> 3 = 1.
        let mut p = row([0, 0, 50, 50, 53, 50, 0, 0]);
        simple_edge(&mut p, 4, 1, 8, 1, 255);
        assert_eq!(p, [0, 0, 50, 51, 52, 50, 0, 0]);
    }

    #[test]
    fn subblock_filter_known_answers() {
        // No high edge variance: w = 3 * 8 = 24, f1 = f2 = 3, and the outer
        // pixels move by (3 + 1) >> 1 = 2.
        let l = EdgeLimits { edge: 255, interior: 255, hev_threshold: 20 };
        let mut p = row([100, 100, 100, 100, 108, 108, 108, 108]);
        normal_subblock_edge(&mut p, 4, 1, 8, 1, l);
        assert_eq!(p, [100, 100, 102, 103, 105, 106, 108, 108]);
        // High edge variance (|q1 - q0| = 8 > 4): the outer taps join,
        // w = clamp(p1 - q1) + 3 * (q0 - p0) = -16 + 24 = 8, f1 = 1, f2 = 1,
        // and the outer pixels stay.
        let l = EdgeLimits { edge: 255, interior: 255, hev_threshold: 4 };
        let mut p = row([100, 100, 100, 100, 108, 116, 116, 116]);
        normal_subblock_edge(&mut p, 4, 1, 8, 1, l);
        assert_eq!(p, [100, 100, 100, 101, 107, 116, 116, 116]);
        // An interior difference above the interior limit disables it.
        let l = EdgeLimits { edge: 255, interior: 7, hev_threshold: 4 };
        let mut p = row([100, 100, 100, 100, 108, 116, 116, 116]);
        normal_subblock_edge(&mut p, 4, 1, 8, 1, l);
        assert_eq!(p, [100, 100, 100, 100, 108, 116, 116, 116]);
    }

    #[test]
    fn mb_filter_known_answers() {
        // No high edge variance: w = (p1 - q1) + 3 * (q0 - p0) = -16 + 48
        // = 32, and the three pairs move by (63 + 32 * 27) >> 7 = 7,
        // (63 + 32 * 18) >> 7 = 4 and (63 + 32 * 9) >> 7 = 2.
        let l = EdgeLimits { edge: 255, interior: 255, hev_threshold: 20 };
        let mut p = row([100, 100, 100, 100, 116, 116, 116, 116]);
        normal_mb_edge(&mut p, 4, 1, 8, 1, l);
        assert_eq!(p, [100, 102, 104, 107, 109, 112, 114, 116]);
        // High edge variance: as the simple filter, w = -16 + 24 = 8, so
        // only p0 and q0 move, by 1.
        let l = EdgeLimits { edge: 255, interior: 255, hev_threshold: 4 };
        let mut p = row([100, 100, 100, 100, 108, 116, 116, 116]);
        normal_mb_edge(&mut p, 4, 1, 8, 1, l);
        assert_eq!(p, [100, 100, 100, 101, 107, 116, 116, 116]);
    }

    #[test]
    fn horizontal_and_vertical_edges_agree() {
        // Filtering the transposed picture gives the transposed result.
        let mut rng = Rng(3);
        for i in 0..2000 {
            let mut a = [0u8; 256];
            rng.fill(&mut a);
            let mut b = [0u8; 256];
            for (j, v) in b.iter_mut().enumerate() {
                *v = a[(j % 16) * 16 + j / 16];
            }
            let l = EdgeLimits { edge: (rng.next() >> 8) as u8, interior: (rng.next() >> 8) as u8 & 63, hev_threshold: (i % 4) as u8 };
            let count = [16, 8, 5, 1][i % 4];
            match i % 3 {
                0 => {
                    normal_mb_edge(&mut a, 8, 1, 16, count, l);
                    normal_mb_edge(&mut b, 8 * 16, 16, 1, count, l);
                }
                1 => {
                    normal_subblock_edge(&mut a, 8, 1, 16, count, l);
                    normal_subblock_edge(&mut b, 8 * 16, 16, 1, count, l);
                }
                _ => {
                    simple_edge(&mut a, 8, 1, 16, count, l.edge);
                    simple_edge(&mut b, 8 * 16, 16, 1, count, l.edge);
                }
            }
            for (j, &v) in b.iter().enumerate() {
                assert_eq!(v, a[(j % 16) * 16 + j / 16], "case {i}, pixel {j}");
            }
        }
    }

    #[test]
    fn zero_offset_copies() {
        let mut rng = Rng(4);
        for predict in [sixtap as Predict, bilinear] {
            for (w, h) in [(4, 4), (8, 4), (4, 8), (8, 8), (16, 8), (8, 16), (16, 16)] {
                let mut src = [0u8; 1024];
                rng.fill(&mut src);
                let mut dst = [0xAAu8; 576];
                predict(&src, 8 * 32 + 8, 32, &mut dst, 4 * 24 + 4, 24, w, h, 0, 0);
                for (i, &v) in dst.iter().enumerate() {
                    let (y, x) = (i / 24, i % 24);
                    let inside = (4..4 + h).contains(&y) && (4..4 + w).contains(&x);
                    let expect = if inside { src[(y + 4) * 32 + x + 4] } else { 0xAA };
                    assert_eq!(v, expect, "{w}x{h} pixel {i}");
                }
            }
        }
    }

    #[test]
    fn flat_source_predicts_flat() {
        // Every filter's taps sum to 128, so a constant comes back unchanged.
        for predict in [sixtap as Predict, bilinear] {
            for level in [0u8, 1, 127, 128, 254, 255] {
                let src = [level; 1024];
                for mx in 0..8 {
                    for my in 0..8 {
                        let mut dst = [0u8; 256];
                        predict(&src, 8 * 32 + 8, 32, &mut dst, 0, 16, 16, 16, mx, my);
                        assert_eq!(dst, [level; 256]);
                    }
                }
            }
        }
    }

    #[test]
    fn half_pel_known_answers() {
        // A vertical step from 0 to 200 between columns 9 and 10.
        let mut src = [0u8; 1024];
        for (i, v) in src.iter_mut().enumerate() {
            *v = if i % 32 >= 10 { 200 } else { 0 };
        }
        // Half a pixel right of column 9: the six-tap filter gives
        // (200 * (77 - 16 + 3) + 64) >> 7 = 100 and bilinear (200 * 64 + 64) >> 7 = 100;
        // one column left, six-tap undershoots to (200 * (3 - 16) + 64) >> 7 < 0 = 0
        // and one column right it overshoots: (200 * (77 + 77 - 16 + 3) + 64) >> 7 = 220.
        let mut dst = [0u8; 16];
        sixtap(&src, 8 * 32 + 8, 32, &mut dst, 0, 4, 4, 4, 4, 0);
        assert_eq!(dst[..4], [0, 100, 220, 195]);
        bilinear(&src, 8 * 32 + 8, 32, &mut dst, 0, 4, 4, 4, 4, 0);
        assert_eq!(dst[..4], [0, 100, 200, 200]);
        // The same step seen vertically, through the transposed picture.
        let mut t = [0u8; 1024];
        for (i, v) in t.iter_mut().enumerate() {
            *v = src[(i % 32) * 32 + i / 32];
        }
        sixtap(&t, 8 * 32 + 8, 32, &mut dst, 0, 4, 4, 4, 0, 4);
        assert_eq!([dst[0], dst[4], dst[8], dst[12]], [0, 100, 220, 195]);
        bilinear(&t, 8 * 32 + 8, 32, &mut dst, 0, 4, 4, 4, 0, 4);
        assert_eq!([dst[0], dst[4], dst[8], dst[12]], [0, 100, 200, 200]);
    }

    // The real kernels against the plain ones.

    #[test]
    fn idct_matches_reference() {
        let mut rng = Rng(6);
        const CORNERS: [i16; 7] = [i16::MIN, i16::MIN + 1, -1, 0, 1, i16::MAX - 1, i16::MAX];
        for i in 0..400_000 {
            let mut c = [0i16; 16];
            for v in &mut c {
                *v = if i % 5 == 4 { CORNERS[(rng.next() >> 20) as usize % 7] } else { rng.coef(i % 5) };
            }
            let mut a = [0u8; 64];
            rng.fill(&mut a);
            let mut b = a;
            idct_add(&c, &mut a, 18, 8);
            reference::idct_add(&c, &mut b, 18, 8);
            assert_eq!(a, b, "coefficients {c:?}");
        }
    }

    /// Geometries for a 48x48 picture with up to 40 positions: a vertical
    /// edge, a horizontal edge, and two that no caller uses but the contract
    /// allows (every other pixel across the edge; a diagonal step).
    const GEOMETRIES: [(usize, usize, usize); 4] = [(4 * 48 + 8, 1, 48), (8 * 48 + 4, 48, 1), (4 * 48 + 8, 2, 48), (8 * 48 + 4, 47, 1)];

    #[test]
    fn loop_filters_match_reference() {
        let mut rng = Rng(7);
        let mut changed = 0usize;
        for i in 0..120_000usize {
            let mut a = [0u8; 48 * 48];
            rng.fill(&mut a);
            let before = a;
            let mut b = a;
            let (at, step, along) = GEOMETRIES[i % 4];
            let r = rng.next();
            let count = (r % 41) as usize;
            // Every edge limit, every plausible interior limit and
            // threshold, and now and then any byte at all.
            let l = EdgeLimits {
                edge: (i / 4 % 256) as u8,
                interior: if r >> 8 & 7 == 0 { (r >> 16) as u8 } else { (i / 1024 % 128) as u8 },
                hev_threshold: if r >> 11 & 7 == 0 { (r >> 24) as u8 } else { (r >> 24) as u8 & 3 },
            };
            match r >> 32 & 3 {
                0 => {
                    normal_mb_edge(&mut a, at, step, along, count, l);
                    reference::normal_mb_edge(&mut b, at, step, along, count, l);
                }
                1 => {
                    normal_subblock_edge(&mut a, at, step, along, count, l);
                    reference::normal_subblock_edge(&mut b, at, step, along, count, l);
                }
                _ => {
                    simple_edge(&mut a, at, step, along, count, l.edge);
                    reference::simple_edge(&mut b, at, step, along, count, l.edge);
                }
            }
            assert!(a == b, "case {i}: {l:?}, step {step}, along {along}, count {count}");
            changed += usize::from(a != before);
        }
        assert!(changed > 20_000, "only {changed} cases filtered anything");
    }

    #[test]
    fn loop_filter_arithmetic_matches_reference() {
        // Every p0 and q0 against a spread of p1 and q1, with the masks wide
        // open so that the saturating arithmetic is what gets compared. The
        // outer pixels repeat p1 and q1. Each call is one horizontal edge
        // 256 positions long, with q0 running along it.
        let spread: Vec<u8> = (0..=255).step_by(15).collect();
        for &p1 in &spread {
            for &q1 in &spread {
                for p0 in 0..=255u8 {
                    let mut a = [0u8; 8 * 256];
                    for (i, v) in a.iter_mut().enumerate() {
                        *v = match i / 256 {
                            0..=2 => p1,
                            3 => p0,
                            4 => (i % 256) as u8,
                            _ => q1,
                        };
                    }
                    for hev_threshold in [0, 3, 255] {
                        let l = EdgeLimits { edge: 255, interior: 255, hev_threshold };
                        let (mut x, mut y) = (a, a);
                        normal_mb_edge(&mut x, 4 * 256, 256, 1, 256, l);
                        reference::normal_mb_edge(&mut y, 4 * 256, 256, 1, 256, l);
                        assert!(x == y, "mb: p1 {p1}, p0 {p0}, q1 {q1}, threshold {hev_threshold}");
                        let (mut x, mut y) = (a, a);
                        normal_subblock_edge(&mut x, 4 * 256, 256, 1, 256, l);
                        reference::normal_subblock_edge(&mut y, 4 * 256, 256, 1, 256, l);
                        assert!(x == y, "subblock: p1 {p1}, p0 {p0}, q1 {q1}, threshold {hev_threshold}");
                    }
                    let (mut x, mut y) = (a, a);
                    simple_edge(&mut x, 4 * 256, 256, 1, 256, 255);
                    reference::simple_edge(&mut y, 4 * 256, 256, 1, 256, 255);
                    assert!(x == y, "simple: p1 {p1}, p0 {p0}, q1 {q1}");
                }
            }
        }
    }

    #[test]
    fn edge_limit_matches_reference() {
        // The edge test for every limit and every pair of differences.
        for limit in 0..=255u8 {
            for d0 in 0..=255u8 {
                let mut a = [0u8; 4 * 256];
                for (i, v) in a.iter_mut().enumerate() {
                    *v = match i / 256 {
                        0 => (i % 256) as u8,
                        1 => d0,
                        _ => 0,
                    };
                }
                let mut b = a;
                simple_edge(&mut a, 2 * 256, 256, 1, 256, limit);
                reference::simple_edge(&mut b, 2 * 256, 256, 1, 256, limit);
                assert!(a == b, "limit {limit}, |p0 - q0| {d0}");
            }
        }
    }

    #[test]
    fn prediction_matches_reference() {
        let mut rng = Rng(8);
        for (predict, plain) in [(sixtap as Predict, reference::sixtap as Predict), (bilinear, reference::bilinear)] {
            for w in [4, 8, 16] {
                for h in [4, 8, 16] {
                    for mx in 0..8 {
                        for my in 0..8 {
                            for _ in 0..12 {
                                let mut src = [0u8; 1024];
                                rng.fill(&mut src);
                                let mut a = [0xAAu8; 576];
                                let mut b = a;
                                predict(&src, 8 * 32 + 8, 32, &mut a, 4 * 24 + 4, 24, w, h, mx, my);
                                plain(&src, 8 * 32 + 8, 32, &mut b, 4 * 24 + 4, 24, w, h, mx, my);
                                assert!(a == b, "{w}x{h} at ({mx}, {my})");
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn prediction_reads_only_what_it_needs() {
        // A plane holding exactly the pixels the contract names: the block,
        // plus 2 before and 3 after for the six-tap filter, or 1 after for
        // bilinear, in each direction that has an offset.
        let mut rng = Rng(9);
        for (predict, before, after) in [(sixtap as Predict, 2, 3), (bilinear, 0, 1)] {
            for (w, h) in [(4, 4), (8, 8), (16, 16), (16, 8), (8, 16)] {
                for mx in 0..8u8 {
                    for my in 0..8u8 {
                        let (left, right) = if mx != 0 { (before, after) } else { (0, 0) };
                        let (above, below) = if my != 0 { (before, after) } else { (0, 0) };
                        let stride = left + w + right;
                        let mut src = vec![0u8; stride * (above + h + below)];
                        rng.fill(&mut src);
                        // The last row may stop where the last pixel read does.
                        let mut dst = vec![0u8; w * h];
                        predict(&src, above * stride + left, stride, &mut dst, 0, w, w, h, mx, my);
                    }
                }
            }
        }
    }

    /// Timings: `cargo test --release -p airsplice-vp8 -- --ignored --nocapture timing`.
    #[test]
    #[ignore]
    fn timing() {
        use std::hint::black_box;
        use std::time::Instant;

        const STRIDE: usize = 32;
        const CALLS: usize = 64;

        /// Smooth content, so the loop filters take their filtering paths.
        fn picture() -> Vec<u8> {
            let mut rng = Rng(5);
            (0..STRIDE * (16 * CALLS + 16)).map(|i| (120 + (i % STRIDE) / 4 + (rng.next() % 5) as usize) as u8).collect()
        }

        /// Times `run` at 64 separate places in a fresh picture, so that no
        /// call works on pixels another has just written, and prints the
        /// best average of many such rounds, to shrug off a busy machine.
        fn time(name: &str, mut run: impl FnMut(&mut [u8], usize)) {
            let fresh = picture();
            let mut pic = fresh.clone();
            let mut best = f64::MAX;
            for _ in 0..3000 {
                pic.copy_from_slice(&fresh);
                let t = Instant::now();
                for i in 0..CALLS {
                    run(black_box(&mut pic), (16 * i + 8) * STRIDE + 8);
                }
                best = best.min(t.elapsed().as_secs_f64() * 1e9 / CALLS as f64);
            }
            println!("{name:36} {best:6.1} ns");
        }

        let mut rng = Rng(5);
        let mut c = [0i16; 16];
        for v in &mut c {
            *v = rng.coef(0);
        }
        let l = EdgeLimits { edge: 60, interior: 20, hev_threshold: 2 };

        time("idct_add", |pic, at| idct_add(black_box(&c), pic, at, STRIDE));
        time("idct_dc_add", |pic, at| idct_dc_add(black_box(77), pic, at, STRIDE));
        time("iwht", |_, _| {
            black_box(iwht(black_box(&c)));
        });
        for (name, step, along) in [("vertical edge", 1, STRIDE), ("horizontal edge", STRIDE, 1)] {
            for count in [16, 8] {
                time(&format!("normal_mb_edge {name}, {count}"), |pic, at| normal_mb_edge(pic, at, step, along, count, l));
                time(&format!("normal_subblock_edge {name}, {count}"), |pic, at| normal_subblock_edge(pic, at, step, along, count, l));
            }
            time(&format!("simple_edge {name}, 16"), |pic, at| simple_edge(pic, at, step, along, 16, 60));
        }
        let src = picture();
        for (predict, name) in [(sixtap as Predict, "sixtap"), (bilinear, "bilinear")] {
            for (offsets, mx, my) in [("xy", 3, 5), ("x", 3, 0), ("y", 0, 5)] {
                for size in [16, 8, 4] {
                    time(&format!("{name} {size}x{size} {offsets}"), |dst, at| {
                        predict(black_box(&src), at, STRIDE, dst, at, STRIDE, size, size, black_box(mx), black_box(my))
                    });
                }
            }
        }
    }
}
