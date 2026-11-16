//! Overdraw culling: a primitive that a later opaque rectangle hides
//! entirely is not drawn.
//!
//! Every frame is drawn whole, in paint order. Screens stack opaque
//! backgrounds (the window's, a page's, a scroll view's, a page kept
//! under the one shown), so a frame can shade the window's area several
//! times over. A GPU does not notice; the software rasterizer (WARP, on a
//! machine without a GPU) shades every one of those pixels on the CPU, and
//! that is most of its frame. Culling therefore runs on software adapters
//! only ([`wanted`]); on a GPU the overdraw it saves was not measured.
//!
//! An *occluder* paints every pixel of its rectangle opaquely: a quad with
//! a solid background at alpha 1, square corners, and either no border or
//! an opaque one (a dashed border's gaps show the opaque background), or an
//! image (polychrome sprite) drawn at opacity 1 with square corners whose
//! every pixel is opaque ([`crate::fast::opaque_tiles`]). Square means
//! radii of exactly zero, as the quad shader's fast path tests them.
//! Its rectangle is its bounds within its content mask; a bordered quad's
//! is inset by the pixel its antialiased outer edge blends over, an image's
//! by the band where bilinear sampling blends in the atlas around its tile.
//!
//! The rasterizer covers a pixel when the pixel's centre is inside the
//! primitive's rectangle, so a primitive whose whole extent lies inside an
//! occluder's rectangle, and which is painted before it (a lower draw
//! order), changes no pixel the frame shows: it is skipped.
//!
//! A quad that is only partly covered is cut instead, when that cannot
//! change a pixel: a *plain* quad (a solid background, square corners,
//! no border) looks the same through any content mask within its bounds,
//! so it is drawn as the pieces of its rectangle that no later occluder
//! covers ([`Occlusion::pieces`]), each a copy of it masked to the piece.
//! Nested backgrounds (a window, its content area, the page in it) then
//! shade each pixel once.
//!
//! Only the largest few occluders are kept ([`MAX_OCCLUDERS`]). Quads,
//! shadows, images and untransformed glyphs are culled; a culled batch is
//! drawn as the runs of primitives left in it. Nothing is culled while a
//! mask is bound (rounded clip groups), where an occluder's own edge is
//! antialiased by the mask.

use std::ops::Range;

use gpui::{
    Bounds, ContentMask, Corners, Edges, IsZero, PolychromeSprite, Quad, ScaledPixels, Scene,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ADAPTER_DESC1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIAdapter1,
};

/// How far past a clip edge on its right or bottom a primitive is taken to
/// reach (see [`Rect::reached`]).
const EDGE_EPSILON: f32 = 1.0 / 64.0;
/// How many occluders a frame keeps (the largest).
const MAX_OCCLUDERS: usize = 16;
/// Smaller opaque quads hide too little to be worth testing against.
const MIN_OCCLUDER_AREA: f32 = 64.0 * 64.0;
/// A quad cut into more pieces than this is drawn whole.
const MAX_PIECES: usize = 16;

/// Whether `desc` is a software adapter: flagged as one, or the Microsoft
/// Basic Render Driver (WARP behind DXGI), which is also enumerated without
/// the flag. `DirectXRenderer::gpu_specs` reports the same answer.
pub(crate) fn is_software_adapter(desc: &DXGI_ADAPTER_DESC1) -> bool {
    desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0
        || (desc.VendorId, desc.DeviceId) == (0x1414, 0x8c)
}

/// Whether frames on `adapter` cull overdraw: on a software adapter, where
/// every pixel shaded costs CPU time. `GPUI_OVERDRAW_CULLING=1` turns it on
/// everywhere, `=0` off.
pub(crate) fn wanted(adapter: &IDXGIAdapter1) -> bool {
    match std::env::var("GPUI_OVERDRAW_CULLING").as_deref() {
        Ok("1") => return true,
        Ok("0") => return false,
        _ => {}
    }
    // SAFETY: a query on a live adapter.
    unsafe { adapter.GetDesc1() }.is_ok_and(|desc| is_software_adapter(&desc))
}

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

    /// What a primitive at `bounds` drawn through `mask` may cover. Where
    /// the mask cuts it on the right or at the bottom, the clip keeps a
    /// pixel whose centre lies exactly on the mask's edge, while the
    /// rasterizer's top-left rule leaves a centre on an occluder's right or
    /// bottom edge out of the occluder: such an edge reaches a hair further,
    /// so that an occluder ending there neither hides nor cuts it there.
    fn reached(bounds: &Bounds<ScaledPixels>, mask: &ContentMask<ScaledPixels>) -> Rect {
        let r = Rect::of(bounds, mask);
        let b = Rect::of(bounds, &ContentMask { bounds: *bounds });
        Rect {
            x1: if r.x1 < b.x1 {
                r.x1 + EDGE_EPSILON
            } else {
                r.x1
            },
            y1: if r.y1 < b.y1 {
                r.y1 + EDGE_EPSILON
            } else {
                r.y1
            },
            ..r
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

    /// This rectangle within `other`.
    fn within(&self, other: &Rect) -> Rect {
        Rect {
            x0: self.x0.max(other.x0),
            y0: self.y0.max(other.y0),
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
        }
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

/// Whether every corner radius is exactly zero, as the quad shader's fast
/// path tests (a negative radius takes its antialiased path).
fn square(c: &Corners<ScaledPixels>) -> bool {
    c.top_left.is_zero()
        && c.top_right.is_zero()
        && c.bottom_left.is_zero()
        && c.bottom_right.is_zero()
}

/// Whether every border width is exactly zero, as the quad shader's fast
/// path tests (a negative width takes its antialiased path).
fn borderless(w: &Edges<ScaledPixels>) -> bool {
    w.top.is_zero() && w.right.is_zero() && w.bottom.is_zero() && w.left.is_zero()
}

/// Whether `quad` looks the same through any content mask within its
/// bounds: a solid background, square corners and no border.
fn is_plain(quad: &Quad) -> bool {
    square(&quad.corner_radii)
        && borderless(&quad.border_widths)
        && quad.background.as_solid().is_some()
}

/// Whether `quad` paints every pixel of its rectangle opaquely.
pub(crate) fn is_occluder(quad: &Quad) -> bool {
    square(&quad.corner_radii)
        && quad
            .background
            .as_solid()
            .is_some_and(|color| color.a >= 1.0)
        && (borderless(&quad.border_widths) || quad.border_color.a >= 1.0)
}

/// Whether `sprite` covers its rectangle as its pixels do: drawn at full
/// opacity with square corners.
fn is_image_occluder(sprite: &PolychromeSprite) -> bool {
    sprite.opacity >= 1.0 && square(&sprite.corner_radii)
}

/// `bounds` within `mask`, its own edges (not the mask's) moved in by
/// `margin_x` and `margin_y`.
fn inset_own_edges(
    bounds: &Bounds<ScaledPixels>,
    mask: &ContentMask<ScaledPixels>,
    margin_x: f32,
    margin_y: f32,
) -> Rect {
    let r = Rect::of(bounds, mask);
    let b = Rect::of(bounds, &ContentMask { bounds: *bounds });
    Rect {
        x0: r.x0.max(b.x0 + margin_x),
        y0: r.y0.max(b.y0 + margin_y),
        x1: r.x1.min(b.x1 - margin_x),
        y1: r.y1.min(b.y1 - margin_y),
    }
}

/// The rectangle an opaque image covers opaquely: its bounds within its
/// mask, less the band along its edges where bilinear sampling reaches the
/// atlas texels around its tile (half a texel at the scale it is drawn,
/// and a pixel more).
fn image_rect(sprite: &PolychromeSprite) -> Rect {
    let texels = sprite.tile.bounds.size;
    let scale = |drawn: f32, texels: i32| drawn / texels.max(1) as f32;
    let margin_x = 0.5 * scale(sprite.bounds.size.width.0, texels.width.0) + 1.0;
    let margin_y = 0.5 * scale(sprite.bounds.size.height.0, texels.height.0) + 1.0;
    inset_own_edges(&sprite.bounds, &sprite.content_mask, margin_x, margin_y)
}

/// The rectangle an occluding quad covers opaquely. A borderless one takes
/// the quad shader's fast path: every pixel whose centre is inside is the
/// background, unblended. A bordered one goes through the shader's distance
/// field, which antialiases its outer edge over the half pixel inside it
/// (`saturate(0.5 - outer_sdf)`): at fractional bounds those pixels are
/// translucent, so its own edges are moved in by a pixel.
fn quad_rect(quad: &Quad) -> Rect {
    let margin = if borderless(&quad.border_widths) {
        0.0
    } else {
        1.0
    };
    inset_own_edges(&quad.bounds, &quad.content_mask, margin, margin)
}

impl Occlusion {
    /// The largest occluders among `scene`'s quads and images;
    /// `opaque_image` tells whether a sprite's pixels are all opaque.
    pub(crate) fn of(scene: &Scene, opaque_image: impl Fn(&PolychromeSprite) -> bool) -> Occlusion {
        Occlusion::collect(&scene.quads, &scene.polychrome_sprites, opaque_image)
    }

    #[cfg(test)]
    fn of_quads(quads: &[Quad]) -> Occlusion {
        Occlusion::collect(quads, &[], |_| false)
    }

    fn collect(
        quads: &[Quad],
        sprites: &[PolychromeSprite],
        opaque_image: impl Fn(&PolychromeSprite) -> bool,
    ) -> Occlusion {
        let quads = quads
            .iter()
            .filter(|quad| is_occluder(quad))
            .map(|quad| Occluder {
                order: quad.order,
                rect: quad_rect(quad),
            });
        let images = sprites
            .iter()
            .filter(|sprite| is_image_occluder(sprite) && opaque_image(sprite))
            .map(|sprite| Occluder {
                order: sprite.order,
                rect: image_rect(sprite),
            });
        let mut occluders: Vec<Occluder> = quads
            .chain(images)
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
        self.hides(quad.order, &Rect::reached(&quad.bounds, &quad.content_mask))
    }

    /// The pieces of a plain `quad` that no later occluder covers, when
    /// some occluder covers part of it; `None` when it is drawn whole.
    ///
    /// The quad is cut by what it reaches ([`Rect::reached`]), as
    /// [`Self::hides_quad`] tests it: an occluder ending exactly on a clip
    /// edge leaves a sliver thinner than a pixel, and a quad left with one is
    /// drawn whole rather than masked to it. The pieces are then kept within
    /// the quad's own clip.
    pub(crate) fn pieces(&self, quad: &Quad) -> Option<Vec<Rect>> {
        if !is_plain(quad) {
            return None;
        }
        let clip = Rect::of(&quad.bounds, &quad.content_mask);
        let reach = Rect::reached(&quad.bounds, &quad.content_mask);
        let mut pieces = vec![reach];
        let mut cut = false;
        let mut next = Vec::new();
        for o in self
            .occluders
            .iter()
            .filter(|o| o.order > quad.order && o.rect.intersects(&reach))
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
        if !cut {
            return None;
        }
        let sliver =
            |p: &Rect| p.x1 - p.x0 <= 2.0 * EDGE_EPSILON || p.y1 - p.y0 <= 2.0 * EDGE_EPSILON;
        if pieces.iter().any(sliver) {
            return None;
        }
        Some(
            pieces
                .iter()
                .map(|p| p.within(&clip))
                .filter(|p| !p.is_empty())
                .collect(),
        )
    }

    pub(crate) fn hides_shadow(&self, shadow: &gpui::Shadow) -> bool {
        // As the shadow vertex shader: an inset shadow stays in its
        // element; a drop shadow's tail reaches 3 blur radii out.
        let rect = if shadow.inset != 0 {
            Rect::reached(&shadow.element_bounds, &shadow.content_mask)
        } else {
            Rect::reached(&shadow.bounds, &shadow.content_mask)
                .outset(3.0 * shadow.blur_radius.0.max(0.0))
        };
        self.hides(shadow.order, &rect)
    }

    /// Whether a glyph (a monochrome or subpixel sprite) is hidden; one
    /// drawn through a transformation never is.
    pub(crate) fn hides_glyph(
        &self,
        order: u32,
        bounds: &Bounds<ScaledPixels>,
        content_mask: &ContentMask<ScaledPixels>,
        transformation: &gpui::TransformationMatrix,
    ) -> bool {
        let unit = gpui::TransformationMatrix::unit();
        transformation.rotation_scale == unit.rotation_scale
            && transformation.translation == unit.translation
            && self.hides(order, &Rect::reached(bounds, content_mask))
    }

    pub(crate) fn hides_sprite(&self, sprite: &gpui::PolychromeSprite) -> bool {
        self.hides(
            sprite.order,
            &Rect::reached(&sprite.bounds, &sprite.content_mask),
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
    use gpui::{
        Bounds, ContentMask, Corners, Edges, Quad, ScaledPixels, hsla, point, size,
        solid_background,
    };

    use super::{Occlusion, Rect, is_plain, visible_runs};

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
    fn negative_radii_or_borders_are_not_square_or_borderless() {
        // The quad shader's fast path tests for exactly zero: anything else,
        // negative included, is antialiased.
        let under = quad(1, 100.0, 100.0, 100.0, 100.0, 1.0);
        let mut rounded = quad(2, 0.0, 0.0, 1000.0, 800.0, 1.0);
        rounded.corner_radii.top_left = ScaledPixels(-1.0);
        assert!(!Occlusion::of_quads(&[under, rounded]).hides_quad(&under));
        let mut bordered = quad(2, 99.5, 99.5, 101.0, 101.0, 1.0);
        bordered.border_widths.left = ScaledPixels(-0.5);
        bordered.border_color = hsla(0.0, 0.0, 1.0, 1.0);
        // Inset by a pixel like any bordered occluder: no longer covers `under`.
        assert!(!Occlusion::of_quads(&[under, bordered]).hides_quad(&under));
        assert!(!is_plain(&bordered));
    }

    #[test]
    fn small_covers_are_not_kept() {
        let tiny = quad(5, 0.0, 0.0, 10.0, 10.0, 1.0);
        assert!(Occlusion::of_quads(&[tiny]).is_empty());
    }
}
