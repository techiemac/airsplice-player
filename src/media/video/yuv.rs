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
        let y_row = &p.y[row * p.y_stride..];
        let uv_off = (row >> sy) * p.uv_stride;
        for x in 0..w {
            let y = (y_row[x].into() >> shift) as i32;
            let (u, v) = if mono {
                (128, 128)
            } else {
                let cx = x >> sx;
                ((p.u[uv_off + cx].into() >> shift) as i32, (p.v[uv_off + cx].into() >> shift) as i32)
            };
            pixel(&m, y, u, v, &mut line[x * 4..x * 4 + 4]);
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
}
