//! Frame buffers: three planes, each surrounded by a border so motion
//! vectors may point outside the picture.

/// Luma border in pixels (chroma gets half). Motion vectors are clamped so a
/// predicted block starts at most 19 pixels outside the macroblock-aligned
/// picture, and the six-tap filter reads 2 pixels before and 3 after a block:
/// at most 21 pixels before the picture and 21 after. 32 matches libvpx.
pub const BORDER: usize = 32;

#[derive(Default)]
pub struct Plane {
    pub data: Vec<u8>,
    pub stride: usize,
    /// Offset of the picture's top-left pixel in `data`.
    pub origin: usize,
    /// Macroblock-aligned size of the picture area.
    pub width: usize,
    pub height: usize,
    border: usize,
}

impl Plane {
    fn new(width: usize, height: usize, border: usize) -> Plane {
        let stride = width + 2 * border;
        Plane {
            data: vec![0; stride * (height + 2 * border)],
            stride,
            origin: border * stride + border,
            width,
            height,
            border,
        }
    }

    /// Offset of pixel (`x`, `y`) of the picture.
    #[inline(always)]
    pub fn at(&self, x: usize, y: usize) -> usize {
        self.origin + y * self.stride + x
    }

    /// Replicate the edge pixels of rows `rows` into the left and right
    /// borders.
    fn extend_sides(&mut self, rows: std::ops::Range<usize>) {
        let (w, b) = (self.width, self.border);
        for y in rows {
            let start = self.origin + y * self.stride - b;
            let row = &mut self.data[start..start + w + 2 * b];
            let (left, rest) = row.split_at_mut(b);
            let (picture, right) = rest.split_at_mut(w);
            left.fill(picture[0]);
            right.fill(picture[w - 1]);
        }
    }

    /// Copy the first and last rows, borders included, into the top and
    /// bottom borders.
    fn extend_top_bottom(&mut self) {
        let (s, b) = (self.stride, self.border);
        let (top_border, rest) = self.data.split_at_mut(b * s);
        for row in top_border.chunks_exact_mut(s) {
            row.copy_from_slice(&rest[..s]);
        }
        let (rest, bottom_border) = self.data.split_at_mut((b + self.height) * s);
        let last = &rest[rest.len() - s..];
        for row in bottom_border.chunks_exact_mut(s) {
            row.copy_from_slice(last);
        }
    }
}

#[derive(Default)]
pub struct FrameBuf {
    pub y: Plane,
    pub u: Plane,
    pub v: Plane,
}

impl FrameBuf {
    /// A buffer for `mb_cols` x `mb_rows` macroblocks.
    pub fn new(mb_cols: usize, mb_rows: usize) -> FrameBuf {
        FrameBuf {
            y: Plane::new(mb_cols * 16, mb_rows * 16, BORDER),
            u: Plane::new(mb_cols * 8, mb_rows * 8, BORDER / 2),
            v: Plane::new(mb_cols * 8, mb_rows * 8, BORDER / 2),
        }
    }

    pub fn is_allocated(&self) -> bool {
        !self.y.data.is_empty()
    }

    /// Fill the borders from the finished picture, so later frames can
    /// predict from outside it.
    pub fn extend_borders(&mut self) {
        for p in [&mut self.y, &mut self.u, &mut self.v] {
            p.extend_sides(0..p.height);
            p.extend_top_bottom();
        }
    }
}
