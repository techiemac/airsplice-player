//! The black bars around a picture fitted with `ObjectFit::Contain`, painted
//! only where the picture does not reach. A background under the whole
//! picture would be a full-window fill on every frame, and on a weak GPU
//! (the Raspberry Pi 5's) each one costs a refresh: three of them held
//! full-screen video to 20 frames a second.

use std::sync::Arc;

use gpui::{Bounds, ObjectFit, Pixels, RenderImage, Styled, canvas, fill, point, px, size};

/// Bars for `image` shown with `ObjectFit::Contain` in the bounds this fills.
/// Place it absolutely, behind the image, in the same box.
pub fn bars(image: &Arc<RenderImage>) -> impl gpui::IntoElement {
    let image_size = image.size(0);
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            if image_size.width.0 <= 0 || image_size.height.0 <= 0 {
                window.paint_quad(fill(bounds, gpui::black()));
                return;
            }
            for bar in bar_rects(bounds, ObjectFit::Contain.get_bounds(bounds, image_size)) {
                window.paint_quad(fill(bar, gpui::black()));
            }
        },
    )
    .absolute()
    .inset_0()
    .size_full()
}

/// The parts of `outer` that `inner` (centred inside it) leaves uncovered.
/// Each bar reaches one pixel under the picture, so rounding never leaves a
/// seam; the picture is painted over that pixel.
fn bar_rects(outer: Bounds<Pixels>, inner: Bounds<Pixels>) -> Vec<Bounds<Pixels>> {
    let overlap = px(1.);
    let mut bars = Vec::new();
    let (left, right) = (outer.origin.x, outer.origin.x + outer.size.width);
    let (top, bottom) = (outer.origin.y, outer.origin.y + outer.size.height);
    let (in_left, in_right) = (inner.origin.x, inner.origin.x + inner.size.width);
    let (in_top, in_bottom) = (inner.origin.y, inner.origin.y + inner.size.height);
    if in_top > top + px(0.5) {
        bars.push(Bounds { origin: point(left, top), size: size(outer.size.width, in_top - top + overlap) });
    }
    if in_bottom < bottom - px(0.5) {
        let y = in_bottom - overlap;
        bars.push(Bounds { origin: point(left, y), size: size(outer.size.width, bottom - y) });
    }
    if in_left > left + px(0.5) {
        bars.push(Bounds { origin: point(left, top), size: size(in_left - left + overlap, outer.size.height) });
    }
    if in_right < right - px(0.5) {
        let x = in_right - overlap;
        bars.push(Bounds { origin: point(x, top), size: size(right - x, outer.size.height) });
    }
    bars
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::DevicePixels;

    fn fitted(outer: Bounds<Pixels>, w: i32, h: i32) -> Bounds<Pixels> {
        ObjectFit::Contain.get_bounds(outer, size(DevicePixels(w), DevicePixels(h)))
    }

    fn area(b: &Bounds<Pixels>) -> f32 {
        f32::from(b.size.width) * f32::from(b.size.height)
    }

    #[test]
    fn a_picture_of_the_same_shape_needs_no_bars() {
        let outer = Bounds { origin: point(px(0.), px(0.)), size: size(px(1920.), px(1080.)) };
        assert!(bar_rects(outer, fitted(outer, 1280, 720)).is_empty());
    }

    #[test]
    fn a_wide_picture_gets_bars_above_and_below_only() {
        let outer = Bounds { origin: point(px(10.), px(40.)), size: size(px(1000.), px(1000.)) };
        let inner = fitted(outer, 2000, 1000);
        let bars = bar_rects(outer, inner);
        assert_eq!(bars.len(), 2, "{bars:?}");
        // Together the bars cover everything but the picture, plus the
        // one-pixel overlap under its top and bottom edges.
        let covered: f32 = bars.iter().map(area).sum();
        assert_eq!(covered, area(&outer) - area(&inner) + 2.0 * 1000.0);
        assert_eq!(bars[0].origin, outer.origin);
        assert_eq!(bars[1].origin.y + bars[1].size.height, outer.origin.y + outer.size.height);
    }

    #[test]
    fn a_tall_picture_gets_bars_left_and_right_only() {
        let outer = Bounds { origin: point(px(0.), px(0.)), size: size(px(1920.), px(1080.)) };
        let inner = fitted(outer, 1080, 1920);
        let bars = bar_rects(outer, inner);
        assert_eq!(bars.len(), 2, "{bars:?}");
        assert!(bars.iter().all(|b| b.size.height == outer.size.height));
        let covered: f32 = bars.iter().map(area).sum();
        assert_eq!(covered, area(&outer) - area(&inner) + 2.0 * 1080.0);
    }
}
