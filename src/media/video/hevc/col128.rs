//! The Raspberry Pi HEVC decoder's output layouts, `NV12MT_COL128` (`Nc12`)
//! and `NV12MT_10_COL128` (`Nc30`): two planes (luma, interleaved chroma),
//! each cut into 128-byte-wide columns stored one after another. A column
//! holds every row of its slice of the picture, so its stride is
//! `column_height * 128` (half that for chroma).
//!
//! 8-bit: one byte per sample, 128 pixels per column.
//! 10-bit: three samples per little-endian 32-bit word (bits 0-9, 10-19,
//! 20-29), so 96 pixels per column.

use rayon::prelude::*;

use super::super::yuv;

pub struct Col128<'a> {
    /// Plane data in ordinary (cached) memory: copy it out of the device
    /// mapping first, since those mappings are typically uncached and slow
    /// to read in this access pattern.
    pub luma: &'a [u8],
    pub chroma: &'a [u8],
    pub width: usize,
    pub height: usize,
    /// Rows per luma column (the negotiated, aligned picture height).
    pub column_height: usize,
    pub ten_bit: bool,
}

/// The largest downscale (1, 2 or 4) that keeps the picture at least as big
/// as `limit` (the size it can be shown at), so no visible detail is lost.
pub fn scale_for(width: u32, height: u32, limit: Option<(u32, u32)>) -> usize {
    let Some((lw, lh)) = limit.filter(|&(w, h)| w > 0 && h > 0) else { return 1 };
    [4, 2].into_iter().find(|&s| width / s >= lw && height / s >= lh).map_or(1, |s| s as usize)
}

impl Col128<'_> {
    /// Convert with an integer box-filter downscale (`scale` 1, 2 or 4):
    /// each output pixel averages `scale`x`scale` luma samples and the
    /// chroma samples covering them. Returns (bgra, width, height).
    pub fn to_bgra_scaled(&self, scale: usize) -> (Vec<u8>, usize, usize) {
        match scale {
            0 | 1 => (self.to_bgra(), self.width, self.height),
            2 => self.half_size(),
            _ => self.box_filter(scale),
        }
    }

    /// Scale 2, the common case (4K shown at 1080p), unrolled: the same box
    /// filter as `box_filter`, without temporary arrays. 4:2:0 chroma is
    /// already at the output resolution, so it is read, not averaged.
    fn half_size(&self) -> (Vec<u8>, usize, usize) {
        let (ow, oh) = (self.width / 2, self.height / 2);
        let mut out = vec![0u8; ow * oh * 4];
        let m = yuv::matrix_for(oh);
        let luma_col = self.column_height * 128;
        let chroma_col = luma_col / 2;
        let ten = self.ten_bit;
        let col_px = if ten { 96 } else { 128 };
        let word = |b: &[u8], at: usize| u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
        out.par_chunks_mut(ow * 4).enumerate().for_each(|(oy, line)| {
            for col in 0..self.width.div_ceil(col_px) {
                let a = &self.luma[col * luma_col + 2 * oy * 128..][..128];
                let b = &self.luma[col * luma_col + (2 * oy + 1) * 128..][..128];
                let c = &self.chroma[col * chroma_col + oy * 128..][..128];
                let x0 = col * col_px / 2;
                let n = (col_px.min(self.width - col * col_px) / 2).min(ow - x0);
                let px = &mut line[x0 * 4..(x0 + n) * 4];
                if ten {
                    // Two words per row give six samples, three output pixels;
                    // two chroma words hold their three U/V pairs.
                    let s = |w: u32, k: u32| (w >> (10 * k)) & 0x3ff;
                    for g in 0..n.div_ceil(3) {
                        let (a0, a1, b0, b1) = (word(a, g * 8), word(a, g * 8 + 4), word(b, g * 8), word(b, g * 8 + 4));
                        let (c0, c1) = (word(c, g * 8), word(c, g * 8 + 4));
                        let ys = [
                            s(a0, 0) + s(a0, 1) + s(b0, 0) + s(b0, 1),
                            s(a0, 2) + s(a1, 0) + s(b0, 2) + s(b1, 0),
                            s(a1, 1) + s(a1, 2) + s(b1, 1) + s(b1, 2),
                        ];
                        let uv = [(s(c0, 0), s(c0, 1)), (s(c0, 2), s(c1, 0)), (s(c1, 1), s(c1, 2))];
                        for k in 0..3.min(n - g * 3) {
                            let y = (((ys[k] + 2) / 4) >> 2) as i32;
                            let i = (g * 3 + k) * 4;
                            yuv::pixel(&m, y, (uv[k].0 >> 2) as i32, (uv[k].1 >> 2) as i32, &mut px[i..i + 4]);
                        }
                    }
                } else {
                    for j in 0..n {
                        let y = (a[2 * j] as u32 + a[2 * j + 1] as u32 + b[2 * j] as u32 + b[2 * j + 1] as u32 + 2) / 4;
                        yuv::pixel(&m, y as i32, c[2 * j] as i32, c[2 * j + 1] as i32, &mut px[j * 4..j * 4 + 4]);
                    }
                }
            }
        });
        (out, ow, oh)
    }

    /// General integer box filter (scale 2 or 4); the reference the unrolled
    /// scale-2 path is tested against.
    fn box_filter(&self, scale: usize) -> (Vec<u8>, usize, usize) {
        assert!(scale == 2 || scale == 4, "unsupported scale {scale}");
        let (ow, oh) = (self.width / scale, self.height / scale);
        let mut out = vec![0u8; ow * oh * 4];
        let m = yuv::matrix_for(oh);
        let luma_col = self.column_height * 128;
        let chroma_col = luma_col / 2;
        let col_px = if self.ten_bit { 96 } else { 128 };
        let shift = if self.ten_bit { 2 } else { 0 };
        let half = scale / 2;
        let (luma_n, chroma_n) = ((scale * scale) as u32, (half * half) as u32);
        let unpack = |src: &[u8], dst: &mut [u16; 128]| {
            if self.ten_bit {
                unpack10(src, dst);
            } else {
                for i in 0..128 {
                    dst[i] = src[i] as u16;
                }
            }
        };
        out.par_chunks_mut(ow * 4).enumerate().for_each(|(oy, line)| {
            let mut row = [0u16; 128];
            for col in 0..self.width.div_ceil(col_px) {
                let (mut ls, mut cs) = ([0u32; 128], [0u32; 128]);
                for k in 0..scale {
                    unpack(&self.luma[col * luma_col + (oy * scale + k) * 128..][..128], &mut row);
                    for i in 0..128 {
                        ls[i] += row[i] as u32;
                    }
                }
                for k in 0..half {
                    unpack(&self.chroma[col * chroma_col + (oy * half + k) * 128..][..128], &mut row);
                    for i in 0..128 {
                        cs[i] += row[i] as u32;
                    }
                }
                let x0 = col * col_px;
                let n = col_px.min(self.width - x0) / scale;
                for j in 0..n.min(ow - x0 / scale) {
                    let y: u32 = ls[j * scale..j * scale + scale].iter().sum();
                    // Chroma pairs covering these pixels: `half` of them.
                    let (mut u, mut v) = (0u32, 0u32);
                    for p in j * half..j * half + half {
                        u += cs[p * 2];
                        v += cs[p * 2 + 1];
                    }
                    let avg = |sum: u32, n: u32| (((sum + n / 2) / n) >> shift) as i32;
                    let x = x0 / scale + j;
                    yuv::pixel(&m, avg(y, luma_n), avg(u, chroma_n), avg(v, chroma_n), &mut line[x * 4..x * 4 + 4]);
                }
            }
        });
        (out, ow, oh)
    }

    /// De-tile and colour-convert in one pass, a column at a time per row.
    pub fn to_bgra(&self) -> Vec<u8> {
        let (w, h) = (self.width, self.height);
        let mut out = vec![0u8; w * h * 4];
        let m = yuv::matrix_for(h);
        let luma_col = self.column_height * 128;
        let chroma_col = luma_col / 2;
        let col_px = if self.ten_bit { 96 } else { 128 };
        out.par_chunks_mut(w * 4).enumerate().for_each(|(row, line)| {
            let mut ys = [0u16; 128];
            let mut cs = [0u16; 128];
            for col in 0..w.div_ceil(col_px) {
                let luma_row = &self.luma[col * luma_col + row * 128..][..128];
                let chroma_row = &self.chroma[col * chroma_col + (row / 2) * 128..][..128];
                if self.ten_bit {
                    unpack10(luma_row, &mut ys);
                    unpack10(chroma_row, &mut cs);
                } else {
                    for i in 0..128 {
                        ys[i] = luma_row[i] as u16;
                        cs[i] = chroma_row[i] as u16;
                    }
                }
                let shift = if self.ten_bit { 2 } else { 0 };
                let x0 = col * col_px;
                let n = col_px.min(w - x0);
                // Fixed-size, branch-free arrays so the compiler can use SIMD;
                // the arithmetic is exactly `yuv::pixel`'s.
                let (mut yy, mut uu, mut vv) = ([0i32; 128], [0i32; 128], [0i32; 128]);
                for i in 0..128 {
                    let c = (i / 2) * 2;
                    yy[i] = ((ys[i] >> shift) as i32 - 16) * m.y;
                    uu[i] = (cs[c] >> shift) as i32 - 128;
                    vv[i] = (cs[c + 1] >> shift) as i32 - 128;
                }
                // Channels computed as separate arrays, then interleaved:
                // both loops are simple enough to vectorise.
                let (mut bs, mut gs, mut rs) = ([0u8; 128], [0u8; 128], [0u8; 128]);
                for i in 0..128 {
                    let (y, u, v) = (yy[i], uu[i], vv[i]);
                    bs[i] = ((y + m.bu * u + 8192) >> 14).clamp(0, 255) as u8;
                    gs[i] = ((y - m.gu * u - m.gv * v + 8192) >> 14).clamp(0, 255) as u8;
                    rs[i] = ((y + m.rv * v + 8192) >> 14).clamp(0, 255) as u8;
                }
                for (i, px) in line[x0 * 4..(x0 + n) * 4].chunks_exact_mut(4).enumerate() {
                    px.copy_from_slice(&[bs[i], gs[i], rs[i], 255]);
                }
            }
        });
        out
    }
}

/// 128 bytes of 10-bit samples, three per little-endian word, into 96.
#[inline]
fn unpack10(src: &[u8], dst: &mut [u16; 128]) {
    for (k, word) in src.chunks_exact(4).enumerate() {
        let bits = u32::from_le_bytes([word[0], word[1], word[2], word[3]]);
        dst[k * 3] = (bits & 0x3ff) as u16;
        dst[k * 3 + 1] = ((bits >> 10) & 0x3ff) as u16;
        dst[k * 3 + 2] = ((bits >> 20) & 0x3ff) as u16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::video::yuv::{Planes, Subsampling};

    /// Tile planar 8-bit 4:2:0 into Nc12, the inverse of what the decoder
    /// hands back, so a round trip proves the addressing.
    fn tile8(y: &[u8], u: &[u8], v: &[u8], w: usize, h: usize, col_h: usize) -> (Vec<u8>, Vec<u8>) {
        let cols = w.div_ceil(128);
        let mut luma = vec![0u8; cols * col_h * 128];
        let mut chroma = vec![0u8; cols * col_h / 2 * 128];
        for row in 0..h {
            for x in 0..w {
                luma[(x / 128) * col_h * 128 + row * 128 + x % 128] = y[row * w + x];
            }
        }
        let cw = w.div_ceil(2);
        for row in 0..h.div_ceil(2) {
            for x in 0..cw {
                let at = (x / 64) * (col_h / 2) * 128 + row * 128 + (x % 64) * 2;
                chroma[at] = u[row * cw + x];
                chroma[at + 1] = v[row * cw + x];
            }
        }
        (luma, chroma)
    }

    #[test]
    fn eight_bit_round_trip_across_column_boundaries() {
        // 300 px spans three columns, the last partial; height padded.
        let (w, h, col_h) = (300, 10, 16);
        let y: Vec<u8> = (0..w * h).map(|i| (i % 220 + 16) as u8).collect();
        let cw = w / 2;
        let u: Vec<u8> = (0..cw * h / 2).map(|i| (i % 200 + 20) as u8).collect();
        let v: Vec<u8> = (0..cw * h / 2).map(|i| (255 - i % 200) as u8).collect();
        let (luma, chroma) = tile8(&y, &u, &v, w, h, col_h);
        let got = Col128 { luma: &luma, chroma: &chroma, width: w, height: h, column_height: col_h, ten_bit: false }.to_bgra();
        let want = yuv::to_bgra(&Planes {
            y: &y,
            u: &u,
            v: &v,
            y_stride: w,
            uv_stride: cw,
            width: w,
            height: h,
            subsampling: Subsampling::S420,
            bit_depth: 8,
        });
        assert_eq!(got, want);
    }

    #[test]
    fn scale_never_drops_below_the_display_size() {
        assert_eq!(scale_for(3840, 2160, Some((1920, 1080))), 2);
        assert_eq!(scale_for(3840, 2160, Some((1600, 900))), 2);
        assert_eq!(scale_for(3840, 2160, Some((960, 540))), 4);
        assert_eq!(scale_for(3840, 2160, Some((2560, 1440))), 1);
        assert_eq!(scale_for(1920, 1080, Some((1920, 1080))), 1);
        assert_eq!(scale_for(3840, 2160, None), 1);
    }

    #[test]
    fn two_x_downscale_matches_averaging_then_converting() {
        let (w, h, col_h) = (300, 12, 16);
        let y: Vec<u8> = (0..w * h).map(|i| ((i * 7) % 220 + 16) as u8).collect();
        let cw = w / 2;
        let u: Vec<u8> = (0..cw * h / 2).map(|i| ((i * 3) % 200 + 20) as u8).collect();
        let v: Vec<u8> = (0..cw * h / 2).map(|i| (255 - (i * 5) % 200) as u8).collect();
        let (luma, chroma) = tile8(&y, &u, &v, w, h, col_h);
        let col = Col128 { luma: &luma, chroma: &chroma, width: w, height: h, column_height: col_h, ten_bit: false };
        let (got, ow, oh) = col.to_bgra_scaled(2);
        assert_eq!((ow, oh), (150, 6));
        let m = yuv::matrix_for(oh);
        for oy in 0..oh {
            for ox in 0..ow {
                let ys = [y[2 * oy * w + 2 * ox], y[2 * oy * w + 2 * ox + 1], y[(2 * oy + 1) * w + 2 * ox], y[(2 * oy + 1) * w + 2 * ox + 1]];
                let yavg = (ys.iter().map(|&s| s as u32).sum::<u32>() + 2) / 4;
                let mut want = [0u8; 4];
                yuv::pixel(&m, yavg as i32, u[oy * cw + ox] as i32, v[oy * cw + ox] as i32, &mut want);
                let at = (oy * ow + ox) * 4;
                assert_eq!(&got[at..at + 4], &want, "pixel ({ox}, {oy})");
            }
        }
    }

    /// Random 10-bit column data: the unrolled scale-2 path must match the
    /// general box filter exactly.
    #[test]
    fn unrolled_half_size_matches_the_box_filter() {
        for ten_bit in [false, true] {
            let (w, h, col_h): (usize, usize, usize) = (400, 20, 24);
            let col_px = if ten_bit { 96 } else { 128 };
            let cols = w.div_ceil(col_px);
            let mut seed = 12345u32;
            let mut rand = || {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed
            };
            let mut fill = |len: usize| -> Vec<u8> {
                if ten_bit {
                    (0..len / 4)
                        .flat_map(|_| {
                            let (s0, s1, s2) = (64 + rand_sample(&mut rand), 64 + rand_sample(&mut rand), 64 + rand_sample(&mut rand));
                            (s0 | s1 << 10 | s2 << 20).to_le_bytes()
                        })
                        .collect()
                } else {
                    (0..len).map(|_| 16 + (rand() % 220) as u8).collect()
                }
            };
            let luma = fill(cols * col_h * 128);
            let chroma = fill(cols * col_h / 2 * 128);
            let c = Col128 { luma: &luma, chroma: &chroma, width: w, height: h, column_height: col_h, ten_bit };
            assert_eq!(c.half_size(), c.box_filter(2), "ten_bit = {ten_bit}");
        }
    }

    fn rand_sample(rand: &mut impl FnMut() -> u32) -> u32 {
        rand() % 876
    }

    #[test]
    fn ten_bit_unpacks_three_samples_per_word() {
        // One column, 4 rows of 6 pixels: luma 64..=1023 range values.
        let (w, h, col_h) = (6, 4, 4);
        let pack = |s: [u16; 3]| (s[0] as u32 | (s[1] as u32) << 10 | (s[2] as u32) << 20).to_le_bytes();
        let mut luma = vec![0u8; col_h * 128];
        let mut chroma = vec![0u8; col_h / 2 * 128];
        for row in 0..h {
            luma[row * 128..row * 128 + 4].copy_from_slice(&pack([940, 940, 940]));
            luma[row * 128 + 4..row * 128 + 8].copy_from_slice(&pack([64, 64, 64]));
        }
        for row in 0..h / 2 {
            // U V U | V U V: neutral chroma.
            chroma[row * 128..row * 128 + 4].copy_from_slice(&pack([512, 512, 512]));
            chroma[row * 128 + 4..row * 128 + 8].copy_from_slice(&pack([512, 512, 512]));
        }
        let bgra = Col128 { luma: &luma, chroma: &chroma, width: w, height: h, column_height: col_h, ten_bit: true }.to_bgra();
        // Limited-range white for the first three pixels, black after.
        assert_eq!(&bgra[0..4], &[255, 255, 255, 255]);
        assert_eq!(&bgra[2 * 4..2 * 4 + 4], &[255, 255, 255, 255]);
        assert_eq!(&bgra[3 * 4..3 * 4 + 4], &[0, 0, 0, 255]);
        assert_eq!(&bgra[5 * 4..5 * 4 + 4], &[0, 0, 0, 255]);
    }
}
