//! Rounded overflow clips keep their own padding-edge geometry. Intersecting
//! rectangular culling bounds must never move a corner or give a descendant
//! its ancestor's radius (zed#37480).
//!
//! Scopes refer to the unsorted paint operation stream, rather than GPU batch
//! indices: batching can reorder non-overlapping primitives. Replaying a view
//! copies the scopes owned by that view under the caller's current scope.

use std::ops::Range;

use crate::{
    App, Bounds, Corners, Overflow, Pixels, ScaledPixels, Scene, Size, Style, Window, point, size,
};

/// The curved padding edge, in device pixels. Adjacent borders can have
/// different widths, so the inner corner is generally an ellipse.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Shape {
    pub(crate) bounds: Bounds<ScaledPixels>,
    pub(crate) radii: Corners<Size<ScaledPixels>>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ClipOperation {
    Begin(Shape),
    End,
}

impl ClipOperation {
    pub(crate) fn translated(self, delta: crate::Point<ScaledPixels>) -> Self {
        match self {
            Self::Begin(mut shape) => {
                shape.bounds.origin += delta;
                Self::Begin(shape)
            }
            Self::End => Self::End,
        }
    }

    pub(crate) fn hash(self, hasher: &mut impl std::hash::Hasher) {
        match self {
            Self::Begin(shape) => {
                hasher.write_u8(3);
                for value in [
                    shape.bounds.origin.x,
                    shape.bounds.origin.y,
                    shape.bounds.size.width,
                    shape.bounds.size.height,
                    shape.radii.top_left.width,
                    shape.radii.top_left.height,
                    shape.radii.top_right.width,
                    shape.radii.top_right.height,
                    shape.radii.bottom_right.width,
                    shape.radii.bottom_right.height,
                    shape.radii.bottom_left.width,
                    shape.radii.bottom_left.height,
                ] {
                    hasher.write_u32(value.0.to_bits());
                }
            }
            Self::End => hasher.write_u8(4),
        }
    }
}

impl Shape {
    fn for_style(style: &Style, bounds: Bounds<Pixels>, rem: Pixels, scale: f32) -> Option<Self> {
        // CSS curves overflow's padding edge only when both axes clip.
        if style.overflow.x == Overflow::Visible || style.overflow.y == Overflow::Visible {
            return None;
        }
        // Match the radii used to paint this element's background and border.
        let radii = style
            .corner_radii
            .to_pixels(rem)
            .clamp_radii_for_quad_size(bounds.size);
        if radii.max() <= Pixels::ZERO {
            return None;
        }
        let widths = style.border_widths.to_pixels(rem);
        let inner = |radius: Pixels, horizontal: Pixels, vertical: Pixels| {
            size(
                ScaledPixels(f32::from((radius - horizontal).max(Pixels::ZERO)) * scale),
                ScaledPixels(f32::from((radius - vertical).max(Pixels::ZERO)) * scale),
            )
        };
        Some(Self {
            bounds: Bounds {
                origin: (bounds.origin + point(widths.left, widths.top)).scale(scale),
                size: size(
                    (bounds.size.width - widths.left - widths.right).max(Pixels::ZERO),
                    (bounds.size.height - widths.top - widths.bottom).max(Pixels::ZERO),
                )
                .scale(scale),
            },
            radii: Corners {
                top_left: inner(radii.top_left, widths.left, widths.top),
                top_right: inner(radii.top_right, widths.right, widths.top),
                bottom_right: inner(radii.bottom_right, widths.right, widths.bottom),
                bottom_left: inner(radii.bottom_left, widths.left, widths.bottom),
            },
        })
    }

    /// CPU reference for testing the geometric intersection independently of
    /// antialiasing. Check every corner: an unusually thick opposite border
    /// can cut through a corner's quarter ellipse.
    #[cfg(test)]
    fn contains(&self, position: crate::Point<ScaledPixels>) -> bool {
        let b = self.bounds;
        if b.is_empty() || !b.contains(&position) {
            return false;
        }
        let corners = [
            (b.origin, self.radii.top_left, 1., 1.),
            (b.top_right(), self.radii.top_right, -1., 1.),
            (b.bottom_right(), self.radii.bottom_right, -1., -1.),
            (b.bottom_left(), self.radii.bottom_left, 1., -1.),
        ];
        corners.into_iter().all(|(corner, radius, dx, dy)| {
            let rx = radius.width.0;
            let ry = radius.height.0;
            if rx == 0. || ry == 0. {
                return true;
            }
            let x = (position.x.0 - corner.x.0) * dx;
            let y = (position.y.0 - corner.y.0) * dy;
            x >= rx || y >= ry || ((x - rx) / rx).powi(2) + ((y - ry) / ry).powi(2) <= 1.
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Scope {
    pub(crate) operations: Range<usize>,
    pub(crate) shape: Shape,
    pub(crate) parent: Option<usize>,
}

/// Private scene data; ContentMask and all public primitive layouts stay
/// unchanged. An ordinary rectangular scene never allocates any scope data.
#[derive(Default)]
pub(crate) struct SceneClips {
    pub(crate) scopes: Vec<Scope>,
    active: Vec<usize>,
    pub(crate) compiled: bool,
}

impl SceneClips {
    fn begin(&mut self, operation: usize, shape: Shape) -> usize {
        let index = self.scopes.len();
        self.scopes.push(Scope {
            operations: operation..operation,
            shape,
            parent: self.active.last().copied(),
        });
        self.active.push(index);
        index
    }

    fn end(&mut self, scope: usize, operation: usize) {
        let active = self.active.pop();
        debug_assert_eq!(active, Some(scope));
        self.scopes[scope].operations.end = operation;
    }
}

pub(crate) fn clear(scene: &mut Scene) {
    scene.fast_rounded_clip.scopes.clear();
    scene.fast_rounded_clip.active.clear();
    scene.fast_rounded_clip.compiled = false;
}

/// The continuation paints descendants; the element's own border and drop
/// shadow remain outside this scope. The shape uses the original element
/// bounds, never the already-intersected rectangular content mask.
pub(crate) fn paint_children(
    style: &Style,
    bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
    paint: impl FnOnce(&mut Window, &mut App),
) {
    let Some(shape) = Shape::for_style(style, bounds, window.rem_size(), window.scale_factor())
    else {
        return paint(window, cx);
    };
    record(&mut window.next_frame.scene, ClipOperation::Begin(shape));
    paint(window, cx);
    record(&mut window.next_frame.scene, ClipOperation::End);
}

/// Explicit operation markers distinguish scope ownership even when a parent
/// and its only child paint exactly the same primitive range. Markers change
/// neither the bounds tree nor the primitive draw orders.
pub(crate) fn record(scene: &mut Scene, operation: ClipOperation) {
    scene.fast_rounded_clip.compiled = false;
    let index = scene.len();
    match operation {
        ClipOperation::Begin(shape) => {
            scene.fast_rounded_clip.begin(index, shape);
        }
        ClipOperation::End => {
            let scope = *scene
                .fast_rounded_clip
                .active
                .last()
                .expect("balanced rounded clips");
            scene.fast_rounded_clip.end(scope, index + 1);
        }
    }
    scene
        .paint_operations
        .push(crate::scene::PaintOperation::FastRoundedClip(operation));
}

pub(crate) fn track(operation: ClipOperation, active: &mut Vec<Shape>) -> anyhow::Result<()> {
    match operation {
        ClipOperation::Begin(shape) => active.push(shape),
        ClipOperation::End => {
            anyhow::ensure!(
                active.pop().is_some(),
                "source scene closes a rounded clip that is not open"
            );
        }
    }
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
impl Window {
    /// The complete rendered scene, including clip scopes and scroll layers,
    /// for tests that rasterize through a platform's headless GPU renderer.
    pub fn painted_scene(&self) -> &Scene {
        &self.rendered_frame.scene
    }
}

#[cfg(test)]
mod tests {
    use super::{ClipOperation, SceneClips, Shape, record};
    use crate::{
        Bounds, Corners, Edges, Overflow, Quad, ScaledPixels, Scene, Style, point, px, size,
    };

    fn style(radius: f32) -> Style {
        Style {
            overflow: point(Overflow::Hidden, Overflow::Hidden),
            corner_radii: Corners::all(px(radius)).map(|radius| (*radius).into()),
            ..Default::default()
        }
    }

    fn shape() -> Shape {
        Shape::for_style(
            &style(24.),
            Bounds::new(point(px(0.), px(0.)), size(px(64.), px(64.))),
            px(16.),
            1.,
        )
        .unwrap()
    }

    fn at(x: f32, y: f32) -> crate::Point<ScaledPixels> {
        point(ScaledPixels(x), ScaledPixels(y))
    }

    fn quad() -> Quad {
        let bounds = shape().bounds;
        Quad {
            bounds,
            content_mask: crate::ContentMask { bounds },
            ..Default::default()
        }
    }

    #[test]
    fn rounded_clip_preserves_original_geometry_under_rectangular_cropping() {
        let mut shape = shape();
        shape.bounds.size.height = ScaledPixels(200.);
        let crop = Bounds::new(at(0., 0.), size(ScaledPixels(64.), ScaledPixels(50.)));
        assert!(!shape.contains(at(1., 1.)));
        assert!(shape.contains(at(1., 49.)) && crop.contains(&at(1., 49.)));
        assert!(!crop.contains(&at(32., 51.)));
    }

    #[test]
    fn rounded_clip_does_not_give_an_inset_rect_an_ancestor_radius() {
        let parent = shape();
        let inset = Bounds::new(at(16., 16.), size(ScaledPixels(32.), ScaledPixels(32.)));
        assert!(parent.contains(at(17., 17.)) && inset.contains(&at(17., 17.)));
        assert!(
            !Shape {
                bounds: inset,
                ..parent
            }
            .contains(at(17., 17.))
        );
    }

    #[test]
    fn transparent_unequal_borders_produce_an_elliptical_padding_edge() {
        let mut style = style(24.);
        style.border_widths = Edges {
            left: px(8.).into(),
            top: px(4.).into(),
            right: px(2.).into(),
            bottom: px(6.).into(),
        };
        let shape = Shape::for_style(
            &style,
            Bounds::new(point(px(0.), px(0.)), size(px(64.), px(64.))),
            px(16.),
            2.,
        )
        .unwrap();
        assert_eq!(shape.bounds.origin, at(16., 8.));
        assert_eq!(
            shape.radii.top_left,
            size(ScaledPixels(32.), ScaledPixels(40.))
        );
        assert_eq!(
            shape.radii.bottom_right,
            size(ScaledPixels(44.), ScaledPixels(36.))
        );
        assert!(!shape.contains(at(17., 9.)));
        assert!(shape.contains(at(48., 9.)));
    }

    #[test]
    fn visible_axis_and_square_corners_need_no_rounded_clip() {
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(64.), px(64.)));
        assert!(Shape::for_style(&style(0.), bounds, px(16.), 1.).is_none());
        let mut style = style(24.);
        style.overflow.x = Overflow::Visible;
        assert!(Shape::for_style(&style, bounds, px(16.), 1.).is_none());
    }

    #[test]
    fn replay_reparents_only_scopes_owned_by_the_replayed_view() {
        let mut source = Scene::default();
        record(&mut source, ClipOperation::Begin(shape()));
        source.insert_primitive(quad());
        let start = source.len();
        record(&mut source, ClipOperation::Begin(shape()));
        source.insert_primitive(quad());
        record(&mut source, ClipOperation::End);
        let end = source.len();
        source.insert_primitive(quad());
        record(&mut source, ClipOperation::End);

        let mut target = Scene::default();
        target.insert_primitive(quad());
        record(&mut target, ClipOperation::Begin(shape()));
        target.replay(start..end, &source);
        record(&mut target, ClipOperation::End);
        assert_eq!(target.fast_rounded_clip.scopes.len(), 2);
        assert_eq!(target.fast_rounded_clip.scopes[1].operations, 2..5);
        assert_eq!(target.fast_rounded_clip.scopes[1].parent, Some(0));
        target.clear();
        assert!(target.fast_rounded_clip.scopes.is_empty());
        assert!(target.fast_rounded_clip.active.is_empty());
    }

    #[test]
    fn replay_copies_nested_scopes_and_keeps_siblings_separate() {
        let mut source = Scene::default();
        record(&mut source, ClipOperation::Begin(shape()));
        record(&mut source, ClipOperation::Begin(shape()));
        source.insert_primitive(quad());
        record(&mut source, ClipOperation::End);
        record(&mut source, ClipOperation::End);
        record(&mut source, ClipOperation::Begin(shape()));
        source.insert_primitive(quad());
        record(&mut source, ClipOperation::End);
        let mut target = Scene::default();
        target.replay(0..source.len(), &source);
        let scopes = &target.fast_rounded_clip.scopes;
        assert_eq!(scopes.len(), 3);
        assert_eq!(scopes[0].parent, None);
        assert_eq!(scopes[1].parent, Some(0));
        assert_eq!(scopes[2].parent, None);
        assert_eq!(scopes, &source.fast_rounded_clip.scopes);
    }

    #[test]
    fn replaying_an_only_child_does_not_capture_its_parents_clip() {
        let mut source = Scene::default();
        record(&mut source, ClipOperation::Begin(shape()));
        let child_start = source.len();
        source.insert_primitive(quad());
        let child_end = source.len();
        record(&mut source, ClipOperation::End);

        let mut target = Scene::default();
        target.replay(child_start..child_end, &source);
        assert!(target.fast_rounded_clip.scopes.is_empty());
        assert_eq!(target.quads.len(), 1);
    }

    #[test]
    fn composition_ranges_restore_and_close_enclosing_rounded_clips() {
        let mut source = Scene::default();
        record(&mut source, ClipOperation::Begin(shape()));
        let start = source.len();
        source.insert_primitive(quad());
        let end = source.len();
        record(&mut source, ClipOperation::End);
        let mut surface = Scene::default();
        surface.replay_balanced(start..end, &source).unwrap();
        assert_eq!(surface.fast_rounded_clip.scopes.len(), 1);
        assert_eq!(surface.fast_rounded_clip.scopes[0].shape, shape());
        assert_eq!(surface.fast_rounded_clip.scopes[0].operations, 0..3);
        assert!(surface.fast_rounded_clip.active.is_empty());
    }

    #[test]
    fn scrolling_translates_corner_centers_without_changing_radii() {
        let mut source = Scene::default();
        record(&mut source, ClipOperation::Begin(shape()));
        source.insert_primitive(quad());
        record(&mut source, ClipOperation::End);
        let layer = crate::LayerFrame {
            key: crate::LayerKey(1),
            generation: 1,
            background: crate::Rgba::default(),
            tile_size: 32,
            content: source.into(),
            dirty_tiles: Vec::new(),
        };
        let tile = layer.tile_scene(crate::TileCoord { x: 1, y: 0 });
        let moved = tile.fast_rounded_clip.scopes[0].shape;
        assert_eq!(moved.bounds.origin, at(-32., 0.));
        assert_eq!(moved.radii, shape().radii);
        assert!(tile.fast_rounded_clip.active.is_empty());
    }

    #[test]
    fn changing_only_the_clip_invalidates_scroll_tile_pixels() {
        fn scene(radius: f32) -> Scene {
            let mut scene = Scene::default();
            let mut shape = shape();
            shape.radii = shape
                .radii
                .map(|_| size(ScaledPixels(radius), ScaledPixels(radius)));
            record(&mut scene, ClipOperation::Begin(shape));
            scene.insert_primitive(quad());
            record(&mut scene, ClipOperation::End);
            scene
        }
        let original = scene(24.);
        let changed = scene(8.);
        let hashes =
            |scene: &Scene| crate::fast::layers::tiles::tile_hashes(scene, 32, shape().bounds);
        assert_ne!(hashes(&original), hashes(&changed));
        let hashes = |scene: &Scene| {
            crate::fast::layers::tiles::part_tile_hashes(&scene.paint_operations, 32).0
        };
        assert_ne!(hashes(&original), hashes(&changed));
    }

    #[test]
    fn rectangular_replay_does_not_allocate_clip_state() {
        let mut source = Scene::default();
        source.insert_primitive(quad());
        let mut target = Scene::default();
        target.replay(0..1, &source);
        assert_eq!(target.fast_rounded_clip.scopes.capacity(), 0);
        assert_eq!(target.fast_rounded_clip.active.capacity(), 0);
        assert_eq!(SceneClips::default().scopes.capacity(), 0);
    }
}
