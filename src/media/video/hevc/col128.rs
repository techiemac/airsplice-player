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

impl Col128<'_> {
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
