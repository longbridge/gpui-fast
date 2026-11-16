//! Overdraw culling: a primitive that a later opaque rectangle hides
//! entirely is not drawn.
//!
//! Every frame is drawn whole, in paint order. Screens stack opaque
//! backgrounds (the window's, a page's, a scroll view's, a page kept
//! under the one shown), so a frame can shade the window's area several
//! times over. A GPU does not notice; the software rasterizer (WARP, on a
//! machine without a GPU) shades every one of those pixels on the CPU, and
//! that is most of its frame.
//!
//! An *occluder* is a quad that paints every pixel of its rectangle
//! opaquely: a solid background with alpha 1, square corners, and either
//! no border or an opaque one (a dashed border's gaps show the opaque
//! background). Its rectangle is its bounds within its content mask. The
//! rasterizer covers a pixel when the pixel's centre is inside the
//! primitive's rectangle (an unrounded quad has no antialiased edge), so a
//! primitive whose whole extent lies inside an occluder's rectangle, and
//! which is painted before it (a lower draw order), changes no pixel the
//! frame shows: it is skipped.
//!
//! A quad that is only partly covered is cut instead, when that cannot
//! change a pixel: a *plain* quad (a solid background, square corners,
//! no border) looks the same through any content mask within its bounds,
//! so it is drawn as the pieces of its rectangle that no later occluder
//! covers ([`Occlusion::pieces`]), each a copy of it masked to the piece.
//! Nested backgrounds (a window, its content area, the page in it) then
//! shade each pixel once.
//!
//! Only the largest few occluders are kept ([`MAX_OCCLUDERS`]), and only
//! quads, shadows and polychrome sprites (images) are culled: they are what
//! fills large areas. A culled batch is drawn as the runs of primitives
//! left in it. Nothing is culled while a mask is bound (rounded clip
//! groups), where an occluder's own edge is antialiased by the mask.

use std::ops::Range;

use gpui::{Bounds, ContentMask, Quad, ScaledPixels, Scene};

#[cfg(test)]
thread_local! {
    static DISABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Turns culling off on this thread (tests compare against drawing everything).
#[cfg(test)]
pub(crate) fn set_disabled(disabled: bool) {
    DISABLED.with(|cell| cell.set(disabled));
}

/// Whether frames cull (always, outside tests).
pub(crate) fn enabled() -> bool {
    #[cfg(test)]
    {
        !DISABLED.with(std::cell::Cell::get)
    }
    #[cfg(not(test))]
    {
        true
    }
}

/// How many occluders a frame keeps (the largest).
const MAX_OCCLUDERS: usize = 16;
/// Smaller opaque quads hide too little to be worth testing against.
const MIN_OCCLUDER_AREA: f32 = 64.0 * 64.0;
/// A quad cut into more pieces than this is drawn whole.
const MAX_PIECES: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Rect {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
}

impl Rect {
    fn of(bounds: &Bounds<ScaledPixels>, mask: &ContentMask<ScaledPixels>) -> Rect {
        let m = &mask.bounds;
        Rect {
            x0: bounds.origin.x.0.max(m.origin.x.0),
            y0: bounds.origin.y.0.max(m.origin.y.0),
            x1: (bounds.origin.x.0 + bounds.size.width.0).min(m.origin.x.0 + m.size.width.0),
            y1: (bounds.origin.y.0 + bounds.size.height.0).min(m.origin.y.0 + m.size.height.0),
        }
    }

    fn outset(self, by: f32) -> Rect {
        Rect {
            x0: self.x0 - by,
            y0: self.y0 - by,
            x1: self.x1 + by,
            y1: self.y1 + by,
        }
    }

    fn area(&self) -> f32 {
        (self.x1 - self.x0).max(0.0) * (self.y1 - self.y0).max(0.0)
    }

    fn is_empty(&self) -> bool {
        !(self.x1 > self.x0 && self.y1 > self.y0)
    }

    fn intersects(&self, other: &Rect) -> bool {
        self.x0 < other.x1 && other.x0 < self.x1 && self.y0 < other.y1 && other.y0 < self.y1
    }

    /// This rectangle without `hole`: up to four rectangles (the bands
    /// above and below the hole, and left and right of it between them).
    fn minus(&self, hole: &Rect, out: &mut Vec<Rect>) {
        if !self.intersects(hole) {
            out.push(*self);
            return;
        }
        let (y0, y1) = (hole.y0.max(self.y0), hole.y1.min(self.y1));
        let bands = [
            Rect {
                x0: self.x0,
                y0: self.y0,
                x1: self.x1,
                y1: y0,
            },
            Rect {
                x0: self.x0,
                y0: y1,
                x1: self.x1,
                y1: self.y1,
            },
            Rect {
                x0: self.x0,
                y0,
                x1: hole.x0.max(self.x0),
                y1,
            },
            Rect {
                x0: hole.x1.min(self.x1),
                y0,
                x1: self.x1,
                y1,
            },
        ];
        out.extend(bands.into_iter().filter(|r| !r.is_empty()));
    }

    pub(crate) fn bounds(&self) -> Bounds<ScaledPixels> {
        Bounds {
            origin: gpui::point(ScaledPixels(self.x0), ScaledPixels(self.y0)),
            size: gpui::size(
                ScaledPixels(self.x1 - self.x0),
                ScaledPixels(self.y1 - self.y0),
            ),
        }
    }

    /// Whether `inner` lies within this rectangle (NaN never does).
    fn contains(&self, inner: &Rect) -> bool {
        inner.x0 >= self.x0 && inner.y0 >= self.y0 && inner.x1 <= self.x1 && inner.y1 <= self.y1
    }
}

#[derive(Clone, Copy, Debug)]
struct Occluder {
    order: u32,
    rect: Rect,
}

/// A frame's occluders.
#[derive(Default)]
pub(crate) struct Occlusion {
    occluders: Vec<Occluder>,
}

/// Whether `quad` paints every pixel of its rectangle opaquely.
/// Whether `quad` looks the same through any content mask within its
/// bounds: a solid background, square corners and no border.
fn is_plain(quad: &Quad) -> bool {
    let c = &quad.corner_radii;
    let w = &quad.border_widths;
    c.top_left.0 <= 0.0
        && c.top_right.0 <= 0.0
        && c.bottom_left.0 <= 0.0
        && c.bottom_right.0 <= 0.0
        && w.top.0 <= 0.0
        && w.right.0 <= 0.0
        && w.bottom.0 <= 0.0
        && w.left.0 <= 0.0
        && quad.background.as_solid().is_some()
}

pub(crate) fn is_occluder(quad: &Quad) -> bool {
    let c = &quad.corner_radii;
    let square = c.top_left.0 <= 0.0
        && c.top_right.0 <= 0.0
        && c.bottom_left.0 <= 0.0
        && c.bottom_right.0 <= 0.0;
    let w = &quad.border_widths;
    let bordered = w.top.0 > 0.0 || w.right.0 > 0.0 || w.bottom.0 > 0.0 || w.left.0 > 0.0;
    square
        && quad
            .background
            .as_solid()
            .is_some_and(|color| color.a >= 1.0)
        && (!bordered || quad.border_color.a >= 1.0)
}

impl Occlusion {
    /// The largest occluders among `scene`'s quads.
    pub(crate) fn of(scene: &Scene) -> Occlusion {
        if !enabled() {
            return Occlusion::default();
        }
        Occlusion::of_quads(&scene.quads)
    }

    fn of_quads(quads: &[Quad]) -> Occlusion {
        let mut occluders: Vec<Occluder> = quads
            .iter()
            .filter(|quad| is_occluder(quad))
            .map(|quad| Occluder {
                order: quad.order,
                rect: Rect::of(&quad.bounds, &quad.content_mask),
            })
            .filter(|o| o.rect.area() >= MIN_OCCLUDER_AREA)
            .collect();
        occluders.sort_by(|a, b| b.rect.area().total_cmp(&a.rect.area()));
        occluders.truncate(MAX_OCCLUDERS);
        Occlusion { occluders }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.occluders.is_empty()
    }

    /// Whether a primitive painted at `order` over `rect` is hidden.
    fn hides(&self, order: u32, rect: &Rect) -> bool {
        rect.is_empty()
            || self
                .occluders
                .iter()
                .any(|o| o.order > order && o.rect.contains(rect))
    }

    pub(crate) fn hides_quad(&self, quad: &Quad) -> bool {
        self.hides(quad.order, &Rect::of(&quad.bounds, &quad.content_mask))
    }

    /// The pieces of a plain `quad` that no later occluder covers, when
    /// some occluder covers part of it; `None` when it is drawn whole.
    pub(crate) fn pieces(&self, quad: &Quad) -> Option<Vec<Rect>> {
        if !is_plain(quad) {
            return None;
        }
        let rect = Rect::of(&quad.bounds, &quad.content_mask);
        let mut pieces = vec![rect];
        let mut cut = false;
        let mut next = Vec::new();
        for o in self
            .occluders
            .iter()
            .filter(|o| o.order > quad.order && o.rect.intersects(&rect))
        {
            next.clear();
            for piece in &pieces {
                piece.minus(&o.rect, &mut next);
            }
            std::mem::swap(&mut pieces, &mut next);
            cut = true;
            if pieces.len() > MAX_PIECES {
                return None;
            }
        }
        cut.then_some(pieces)
    }

    pub(crate) fn hides_shadow(&self, shadow: &gpui::Shadow) -> bool {
        // As the shadow vertex shader: an inset shadow stays in its
        // element; a drop shadow's tail reaches 3 blur radii out.
        let rect = if shadow.inset != 0 {
            Rect::of(&shadow.element_bounds, &shadow.content_mask)
        } else {
            Rect::of(&shadow.bounds, &shadow.content_mask)
                .outset(3.0 * shadow.blur_radius.0.max(0.0))
        };
        self.hides(shadow.order, &rect)
    }

    pub(crate) fn hides_sprite(&self, sprite: &gpui::PolychromeSprite) -> bool {
        self.hides(
            sprite.order,
            &Rect::of(&sprite.bounds, &sprite.content_mask),
        )
    }
}

/// The runs of `range` whose primitives are not hidden, in order.
pub(crate) fn visible_runs(
    range: Range<usize>,
    hidden: impl Fn(usize) -> bool,
) -> Vec<Range<usize>> {
    let mut runs = Vec::new();
    let mut start = range.start;
    for i in range.clone() {
        if hidden(i) {
            if i > start {
                runs.push(start..i);
            }
            start = i + 1;
        }
    }
    if range.end > start {
        runs.push(start..range.end);
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Corners, Edges, hsla, point, size, solid_background};

    fn quad(order: u32, x: f32, y: f32, w: f32, h: f32, alpha: f32) -> Quad {
        Quad {
            order,
            border_style: Default::default(),
            bounds: Bounds {
                origin: point(ScaledPixels(x), ScaledPixels(y)),
                size: size(ScaledPixels(w), ScaledPixels(h)),
            },
            content_mask: ContentMask {
                bounds: Bounds {
                    origin: point(ScaledPixels(0.0), ScaledPixels(0.0)),
                    size: size(ScaledPixels(1000.0), ScaledPixels(800.0)),
                },
            },
            background: solid_background(hsla(0.0, 0.0, 0.1, alpha)),
            border_color: Default::default(),
            corner_radii: Corners::default(),
            border_widths: Edges::default(),
        }
    }

    #[test]
    fn a_later_opaque_cover_hides_what_is_under_it() {
        let quads = [
            quad(1, 0.0, 0.0, 1000.0, 800.0, 1.0),
            quad(2, 10.0, 10.0, 50.0, 50.0, 1.0),
            quad(3, 0.0, 0.0, 1000.0, 800.0, 1.0),
        ];
        let occlusion = Occlusion::of_quads(&quads);
        assert!(occlusion.hides_quad(&quads[0]));
        assert!(occlusion.hides_quad(&quads[1]));
        // The topmost cover is drawn.
        assert!(!occlusion.hides_quad(&quads[2]));
    }

    #[test]
    fn translucent_rounded_or_partial_covers_hide_nothing() {
        let under = quad(1, 100.0, 100.0, 100.0, 100.0, 1.0);
        let translucent = quad(2, 0.0, 0.0, 1000.0, 800.0, 0.99);
        let mut rounded = quad(2, 0.0, 0.0, 1000.0, 800.0, 1.0);
        rounded.corner_radii.top_left = ScaledPixels(4.0);
        let partial = quad(2, 150.0, 0.0, 850.0, 800.0, 1.0);
        for cover in [translucent, rounded, partial] {
            let quads = [under, cover];
            assert!(!Occlusion::of_quads(&quads).hides_quad(&under));
        }
    }

    #[test]
    fn a_cover_clipped_by_its_mask_only_hides_inside_the_mask() {
        let under = quad(1, 600.0, 100.0, 100.0, 100.0, 1.0);
        let mut cover = quad(2, 0.0, 0.0, 1000.0, 800.0, 1.0);
        cover.content_mask.bounds.size.width = ScaledPixels(500.0);
        let quads = [under, cover];
        assert!(!Occlusion::of_quads(&quads).hides_quad(&under));
    }

    #[test]
    fn an_opaque_border_keeps_a_cover() {
        let under = quad(1, 100.0, 100.0, 100.0, 100.0, 1.0);
        let mut cover = quad(2, 0.0, 0.0, 1000.0, 800.0, 1.0);
        cover.border_widths.top = ScaledPixels(1.0);
        cover.border_color = hsla(0.0, 0.0, 1.0, 0.5);
        assert!(!Occlusion::of_quads(&[under, cover]).hides_quad(&under));
        cover.border_color = hsla(0.0, 0.0, 1.0, 1.0);
        assert!(Occlusion::of_quads(&[under, cover]).hides_quad(&under));
    }

    #[test]
    fn runs_skip_hidden_primitives() {
        assert_eq!(
            visible_runs(3..10, |i| i == 3 || i == 6 || i == 7),
            vec![4..6, 8..10]
        );
        assert_eq!(visible_runs(0..3, |_| false), vec![0..3]);
        assert!(visible_runs(0..3, |_| true).is_empty());
    }

    fn total(rects: &[Rect]) -> f32 {
        rects.iter().map(Rect::area).sum()
    }

    #[test]
    fn a_partly_covered_plain_quad_keeps_only_its_uncovered_pieces() {
        // Window, then a content area covering all but a 40 px top bar.
        let window = quad(1, 0.0, 0.0, 1000.0, 800.0, 1.0);
        let content = quad(2, 0.0, 40.0, 1000.0, 760.0, 1.0);
        let occlusion = Occlusion::of_quads(&[window, content]);
        let pieces = occlusion.pieces(&window).unwrap();
        assert_eq!(
            pieces,
            vec![Rect {
                x0: 0.0,
                y0: 0.0,
                x1: 1000.0,
                y1: 40.0
            }]
        );
        // The topmost one is not cut.
        assert!(occlusion.pieces(&content).is_none());
    }

    #[test]
    fn a_hole_in_the_middle_leaves_four_pieces_that_tile_the_rest() {
        let under = quad(1, 0.0, 0.0, 1000.0, 800.0, 0.5);
        let over = quad(2, 100.0, 100.0, 200.0, 300.0, 1.0);
        let pieces = Occlusion::of_quads(&[under, over]).pieces(&under).unwrap();
        assert_eq!(pieces.len(), 4);
        assert_eq!(total(&pieces), 1000.0 * 800.0 - 200.0 * 300.0);
        for (i, a) in pieces.iter().enumerate() {
            assert!(!a.intersects(&Rect::of(&over.bounds, &over.content_mask)));
            for b in &pieces[i + 1..] {
                assert!(!a.intersects(b));
            }
        }
    }

    #[test]
    fn rounded_or_bordered_quads_are_not_cut() {
        let mut rounded = quad(1, 0.0, 0.0, 1000.0, 800.0, 1.0);
        rounded.corner_radii.top_left = ScaledPixels(8.0);
        let mut bordered = quad(1, 0.0, 0.0, 1000.0, 800.0, 1.0);
        bordered.border_widths.left = ScaledPixels(1.0);
        let over = quad(2, 0.0, 40.0, 1000.0, 760.0, 1.0);
        assert!(
            Occlusion::of_quads(&[rounded, over])
                .pieces(&rounded)
                .is_none()
        );
        assert!(
            Occlusion::of_quads(&[bordered, over])
                .pieces(&bordered)
                .is_none()
        );
    }

    #[test]
    fn small_covers_are_not_kept() {
        let tiny = quad(5, 0.0, 0.0, 10.0, 10.0, 1.0);
        assert!(Occlusion::of_quads(&[tiny]).is_empty());
    }
}
