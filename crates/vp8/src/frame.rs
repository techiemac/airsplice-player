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

/// Whole rows of one plane, addressed in picture coordinates like `Plane`.
pub struct Rows<'a> {
    pub data: &'a mut [u8],
    pub stride: usize,
    /// Offset in `data` of picture pixel (0, `first_row`).
    origin: usize,
    first_row: usize,
}

impl Rows<'_> {
    /// Offset of picture pixel (`x`, `y`); `y` must be one of these rows.
    #[inline(always)]
    pub fn at(&self, x: usize, y: usize) -> usize {
        self.origin + (y - self.first_row) * self.stride + x
    }
}

/// Some macroblock rows of all three planes.
pub struct FrameRows<'a> {
    pub y: Rows<'a>,
    pub u: Rows<'a>,
    pub v: Rows<'a>,
}

/// A frame buffer lent out a band of macroblock rows at a time, so one
/// thread can reconstruct a row while another filters the rows above it.
pub struct RowSplit<'a> {
    /// Per plane: start of `data`, stride, border and pixel rows per
    /// macroblock row.
    planes: [(*mut u8, usize, usize, usize); 3],
    mb_rows: usize,
    _frame: std::marker::PhantomData<&'a mut FrameBuf>,
}

// SAFETY: a `RowSplit` is only a way to make `FrameRows`, which are plain
// mutable slices; `mb_rows`' contract keeps the ones alive at any moment
// disjoint, wherever they live.
unsafe impl Send for RowSplit<'_> {}
unsafe impl Sync for RowSplit<'_> {}

impl<'a> RowSplit<'a> {
    pub fn new(frame: &'a mut FrameBuf) -> Self {
        let mb_rows = frame.y.height / 16;
        let plane = |p: &mut Plane, mb: usize| {
            assert_eq!(p.height, mb_rows * mb, "planes of one frame");
            (p.data.as_mut_ptr(), p.stride, p.border, mb)
        };
        let planes = [plane(&mut frame.y, 16), plane(&mut frame.u, 8), plane(&mut frame.v, 8)];
        RowSplit { planes, mb_rows, _frame: Default::default() }
    }

    /// Macroblock rows `rows` of each plane, side borders included.
    ///
    /// # Safety
    ///
    /// No two `FrameRows` from one split that are alive at the same time may
    /// share a macroblock row.
    pub unsafe fn mb_rows(&self, rows: std::ops::Range<usize>) -> FrameRows<'a> {
        assert!(rows.start <= rows.end && rows.end <= self.mb_rows, "macroblock rows {rows:?} are outside the frame");
        let band = |&(ptr, stride, border, mb): &(*mut u8, usize, usize, usize)| {
            let (start, end) = ((border + rows.start * mb) * stride, (border + rows.end * mb) * stride);
            // SAFETY: picture rows (checked) lie inside `data`, which holds
            // `border` rows above and below them; the caller guarantees no
            // other live band overlaps; the frame stays borrowed for 'a.
            let data = unsafe { std::slice::from_raw_parts_mut(ptr.add(start), end - start) };
            Rows { data, stride, origin: border, first_row: rows.start * mb }
        };
        FrameRows { y: band(&self.planes[0]), u: band(&self.planes[1]), v: band(&self.planes[2]) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each band is exactly its macroblock rows, side borders included, and
    /// addresses pixels where the whole plane does; neighbouring bands never
    /// share a byte.
    #[test]
    fn bands_cover_their_rows_and_nothing_else() {
        let mut frame = FrameBuf::new(3, 4);
        let whole = |p: &Plane, x, y| p.at(x, y);
        let expected: Vec<_> = [(&frame.y, 16), (&frame.u, 8), (&frame.v, 8)]
            .iter()
            .map(|&(p, mb)| (p.data.as_ptr() as usize, p.stride, p.border, mb, whole(p, 5, 2 * mb + 3)))
            .collect();
        let split = RowSplit::new(&mut frame);
        for r in 0..4 {
            // SAFETY: one band at a time.
            let band = unsafe { split.mb_rows(r..r + 1) };
            let next = (r < 3).then(|| unsafe { split.mb_rows(r + 1..r + 2) }.y.data.as_ptr() as usize);
            for (rows, &(base, stride, border, mb, _)) in [&band.y, &band.u, &band.v].into_iter().zip(&expected) {
                assert_eq!(rows.data.as_ptr() as usize, base + (border + r * mb) * stride);
                assert_eq!(rows.data.len(), mb * stride);
            }
            if let Some(next) = next {
                assert_eq!(band.y.data.as_ptr() as usize + band.y.data.len(), next, "bands {r} and {} touch", r + 1);
            }
        }
        // A pixel two macroblock rows down, through a band of rows 1 and 2.
        let band = unsafe { split.mb_rows(1..3) };
        for (rows, &(base, stride, _, mb, at)) in [&band.y, &band.u, &band.v].into_iter().zip(&expected) {
            assert_eq!(rows.data.as_ptr() as usize + rows.at(5, 2 * mb + 3), base + at);
            assert_eq!(rows.data.len(), 2 * mb * stride);
        }
    }

    #[test]
    #[should_panic(expected = "outside the frame")]
    fn bands_past_the_frame_are_refused() {
        let mut frame = FrameBuf::new(2, 2);
        let split = RowSplit::new(&mut frame);
        let _ = unsafe { split.mb_rows(1..3) };
    }
}
