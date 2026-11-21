//! Planar YUV to BGRA conversion (the pixel format GPUI uploads).

use rayon::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Subsampling {
    /// 4:2:0, chroma halved in both directions.
    S420,
    /// 4:2:2, chroma halved horizontally.
    S422,
    /// 4:4:4, full-resolution chroma.
    S444,
    /// Luma only.
    Mono,
}

pub struct Planes<'a, T> {
    pub y: &'a [T],
    pub u: &'a [T],
    pub v: &'a [T],
    /// Strides in samples (not bytes).
    pub y_stride: usize,
    pub uv_stride: usize,
    pub width: usize,
    pub height: usize,
    pub subsampling: Subsampling,
    pub bit_depth: u32,
}

/// Fixed-point (Q14) limited-range YUV to RGB coefficients.
pub(crate) struct Matrix {
    pub(crate) y: i32,
    pub(crate) rv: i32,
    pub(crate) gu: i32,
    pub(crate) gv: i32,
    pub(crate) bu: i32,
}

/// BT.709 for HD, BT.601 for SD: the convention when the stream carries no
/// colour description.
pub(crate) fn matrix_for(height: usize) -> Matrix {
    let (kr, kb) = if height >= 720 { (0.2126, 0.0722) } else { (0.299, 0.114) };
    let kg = 1.0 - kr - kb;
    let scale = 16384.0;
    let ys = 255.0 / 219.0;
    let cs = 255.0 / 224.0;
    Matrix {
        y: (ys * scale) as i32,
        rv: (2.0 * (1.0 - kr) * cs * scale) as i32,
        gu: (2.0 * (1.0 - kb) * kb / kg * cs * scale) as i32,
        gv: (2.0 * (1.0 - kr) * kr / kg * cs * scale) as i32,
        bu: (2.0 * (1.0 - kb) * cs * scale) as i32,
    }
}

#[inline]
fn clamp8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// One pixel: 8-bit-scale `y`, `u`, `v` (already shifted down from higher
/// bit depths) to BGRA. Every conversion path goes through this.
#[inline(always)]
pub(crate) fn pixel(m: &Matrix, y: i32, u: i32, v: i32, px: &mut [u8]) {
    let (yy, u, v) = ((y - 16) * m.y, u - 128, v - 128);
    px[0] = clamp8((yy + m.bu * u + 8192) >> 14);
    px[1] = clamp8((yy - m.gu * u - m.gv * v + 8192) >> 14);
    px[2] = clamp8((yy + m.rv * v + 8192) >> 14);
    px[3] = 255;
}

/// Pixels per block in `to_bgra` (even, so a 4:2:0 or 4:2:2 block starts on
/// a chroma sample).
const BLOCK: usize = 64;

pub fn to_bgra<T>(p: &Planes<'_, T>) -> Vec<u8>
where
    T: Copy + Into<u32> + Sync,
{
    let (w, h) = (p.width, p.height);
    let mut out = vec![0u8; w * h * 4];
    let m = matrix_for(h);
    let shift = p.bit_depth.saturating_sub(8);
    let (sx, sy) = match p.subsampling {
        Subsampling::S420 => (1, 1),
        Subsampling::S422 => (1, 0),
        Subsampling::S444 | Subsampling::Mono => (0, 0),
    };
    let mono = p.subsampling == Subsampling::Mono;

    out.par_chunks_mut(w * 4).enumerate().for_each(|(row, line)| {
        let y_row = &p.y[row * p.y_stride..][..w];
        let uv_off = (row >> sy) * p.uv_stride;
        let cw = (w + sx) >> sx;
        let (u_row, v_row) = if mono { (&[][..], &[][..]) } else { (&p.u[uv_off..][..cw], &p.v[uv_off..][..cw]) };
        // A block at a time in fixed-size, branch-free arrays so the
        // compiler can use SIMD; the arithmetic is exactly `pixel`'s.
        for x0 in (0..w).step_by(BLOCK) {
            let n = BLOCK.min(w - x0);
            let (mut yy, mut uu, mut vv) = ([0i32; BLOCK], [0i32; BLOCK], [0i32; BLOCK]);
            for (d, s) in yy.iter_mut().zip(&y_row[x0..x0 + n]) {
                *d = ((*s).into() >> shift) as i32;
            }
            if !mono {
                let c0 = x0 >> sx;
                let cn = ((x0 + n + sx) >> sx) - c0;
                let (us, vs) = (&u_row[c0..c0 + cn], &v_row[c0..c0 + cn]);
                if sx == 1 {
                    for (j, (u, v)) in us.iter().zip(vs).enumerate().take(BLOCK / 2) {
                        let (u, v) = (((*u).into() >> shift) as i32, ((*v).into() >> shift) as i32);
                        (uu[2 * j], uu[2 * j + 1], vv[2 * j], vv[2 * j + 1]) = (u, u, v, v);
                    }
                } else {
                    for (j, (u, v)) in us.iter().zip(vs).enumerate() {
                        (uu[j], vv[j]) = (((*u).into() >> shift) as i32, ((*v).into() >> shift) as i32);
                    }
                }
            } else {
                (uu, vv) = ([128; BLOCK], [128; BLOCK]);
            }
            let mut px = [0u32; BLOCK];
            for i in 0..BLOCK {
                let (y, u, v) = ((yy[i] - 16) * m.y + 8192, uu[i] - 128, vv[i] - 128);
                let b = ((y + m.bu * u) >> 14).clamp(0, 255) as u32;
                let g = ((y - m.gu * u - m.gv * v) >> 14).clamp(0, 255) as u32;
                let r = ((y + m.rv * v) >> 14).clamp(0, 255) as u32;
                px[i] = b | g << 8 | r << 16 | 0xff00_0000;
            }
            for (d, s) in line[x0 * 4..(x0 + n) * 4].as_chunks_mut::<4>().0.iter_mut().zip(&px) {
                *d = s.to_le_bytes();
            }
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert(y: u8, u: u8, v: u8, height: usize) -> [u8; 4] {
        let (w, h) = (2, height);
        let ys = vec![y; w * h];
        let cs_u = vec![u; w * h];
        let cs_v = vec![v; w * h];
        let out = to_bgra(&Planes {
            y: &ys,
            u: &cs_u,
            v: &cs_v,
            y_stride: w,
            uv_stride: w,
            width: w,
            height: h,
            subsampling: Subsampling::S444,
            bit_depth: 8,
        });
        out[..4].try_into().unwrap()
    }

    #[test]
    fn limited_range_black_and_white() {
        assert_eq!(convert(16, 128, 128, 2), [0, 0, 0, 255]);
        assert_eq!(convert(235, 128, 128, 2), [255, 255, 255, 255]);
    }

    #[test]
    fn bt601_red_and_bt709_red() {
        // Pure red in limited-range BT.601 is Y=81 U=90 V=240.
        let [b, g, r, _] = convert(81, 90, 240, 2);
        assert!(r >= 253 && g <= 2 && b <= 2, "601 red -> {r},{g},{b}");
        // Pure red in limited-range BT.709 is Y=63 U=102 V=240.
        let [b, g, r, _] = convert(63, 102, 240, 720);
        assert!(r >= 253 && g <= 2 && b <= 2, "709 red -> {r},{g},{b}");
    }

    #[test]
    fn ten_bit_matches_eight_bit() {
        let (w, h) = (2, 2);
        let y = vec![235u16 << 2; w * h];
        let c = vec![128u16 << 2; w * h];
        let out = to_bgra(&Planes {
            y: &y,
            u: &c,
            v: &c,
            y_stride: w,
            uv_stride: w,
            width: w,
            height: h,
            subsampling: Subsampling::S444,
            bit_depth: 10,
        });
        assert_eq!(&out[..4], &[255, 255, 255, 255]);
    }

    /// The block path against `pixel` one sample at a time, on noise, for
    /// every layout, odd sizes, padded strides and widths that end
    /// mid-block.
    #[test]
    fn blocks_match_the_per_pixel_reference() {
        let mut seed = 0x2545f491u32;
        let mut noise = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for sub in [Subsampling::S420, Subsampling::S422, Subsampling::S444, Subsampling::Mono] {
            for (w, h) in [(1, 1), (3, 5), (63, 2), (64, 3), (65, 4), (129, 7), (200, 720)] {
                for depth in [8u32, 10] {
                    let (sx, sy) = match sub {
                        Subsampling::S420 => (1, 1),
                        Subsampling::S422 => (1, 0),
                        _ => (0, 0),
                    };
                    let (ys, cs) = (w + 3, ((w + sx) >> sx) + 5);
                    let ch = (h + sy) >> sy;
                    let max = (1u32 << depth) - 1;
                    let mut plane = |len: usize| (0..len).map(|_| (noise() & max) as u16).collect::<Vec<_>>();
                    let (y, u, v) = (plane(ys * h), plane(cs * ch), plane(cs * ch));
                    let planes = Planes {
                        y: &y,
                        u: &u,
                        v: &v,
                        y_stride: ys,
                        uv_stride: cs,
                        width: w,
                        height: h,
                        subsampling: sub,
                        bit_depth: depth,
                    };
                    let got = to_bgra(&planes);
                    let m = matrix_for(h);
                    let s = depth - 8;
                    let mut want = vec![0u8; w * h * 4];
                    for row in 0..h {
                        for x in 0..w {
                            let c = (row >> sy) * cs + (x >> sx);
                            let (cu, cv) =
                                if sub == Subsampling::Mono { (128, 128) } else { (u[c] as i32 >> s, v[c] as i32 >> s) };
                            let at = (row * w + x) * 4;
                            pixel(&m, y[row * ys + x] as i32 >> s, cu, cv, &mut want[at..at + 4]);
                        }
                    }
                    assert!(got == want, "{sub:?} {w}x{h} {depth}-bit");
                    if depth == 8 {
                        let narrow = |p: &[u16]| p.iter().map(|&s| s as u8).collect::<Vec<_>>();
                        let (y, u, v) = (narrow(&y), narrow(&u), narrow(&v));
                        let eight = to_bgra(&Planes {
                            y: &y,
                            u: &u,
                            v: &v,
                            y_stride: ys,
                            uv_stride: cs,
                            width: w,
                            height: h,
                            subsampling: sub,
                            bit_depth: 8,
                        });
                        assert!(eight == want, "{sub:?} {w}x{h} u8");
                    }
                }
            }
        }
    }
}
