//! Scene damage must be exact: a pixel outside a scene's damage is covered
//! by the same primitives, drawn in the same order, as in the scene before.
//! See [`crate::fast::damage`].

use std::{cell::Cell, rc::Rc, sync::Arc};

use rand::{Rng as _, SeedableRng as _, rngs::StdRng};

use super::oracle::GlyphBoxTextSystem;
use crate::{
    AppContext as _, AtlasTextureId, AtlasTextureKind, AtlasTile, Background, Bounds, ContentMask,
    Context, Corners, DevicePixels, Edges, Entity, Hsla, IntoElement, LayerContent, LayerFrame,
    LayerKey, MonochromeSprite, NoopTextSystem, ParentElement as _, Path, PolychromeSprite,
    Primitive, Quad, Render, ScaledPixels, Scene, Shadow, Size, StyleRefinement, Styled as _,
    SubpixelSprite, TestAppContext, TileCoord, TileId, TransformationMatrix, Underline, Window,
    WindowHandle, div,
    fast::damage::{DamageState, diff_scenes},
    fast::layers::scene::{move_primitive, visible_bounds},
    hsla, layer_tile_id, layer_tile_texture_id, point, px,
    scene::PaddedBool32,
    size,
};

fn bounds(x: f32, y: f32, w: f32, h: f32) -> Bounds<ScaledPixels> {
    Bounds {
        origin: point(ScaledPixels(x), ScaledPixels(y)),
        size: size(ScaledPixels(w), ScaledPixels(h)),
    }
}

fn device(x: i32, y: i32, w: i32, h: i32) -> Bounds<DevicePixels> {
    Bounds {
        origin: point(DevicePixels(x), DevicePixels(y)),
        size: size(DevicePixels(w), DevicePixels(h)),
    }
}

const CANVAS: i32 = 128;

fn canvas_size() -> Size<DevicePixels> {
    size(DevicePixels(CANVAS), DevicePixels(CANVAS))
}

fn whole_mask() -> ContentMask<ScaledPixels> {
    ContentMask {
        bounds: bounds(-50., -50., 300., 300.),
    }
}

fn quad(b: Bounds<ScaledPixels>, color: Hsla) -> Primitive {
    Primitive::Quad(Quad {
        bounds: b,
        content_mask: whole_mask(),
        background: color.into(),
        ..Default::default()
    })
}

fn tile(texture: u32, id: u32, kind: AtlasTextureKind) -> AtlasTile {
    AtlasTile {
        texture_id: AtlasTextureId {
            index: texture,
            kind,
        },
        tile_id: TileId(id),
        padding: 0,
        bounds: device(0, 0, 8, 8),
    }
}

fn mono(b: Bounds<ScaledPixels>, color: Hsla, tile_id: u32) -> MonochromeSprite {
    MonochromeSprite {
        order: 0,
        pad: 0,
        bounds: b,
        content_mask: whole_mask(),
        color,
        tile: tile(0, tile_id, AtlasTextureKind::Monochrome),
        transformation: TransformationMatrix::unit(),
    }
}

fn shadow(b: Bounds<ScaledPixels>, blur: f32, color: Hsla) -> Shadow {
    Shadow {
        order: 0,
        blur_radius: ScaledPixels(blur),
        bounds: b,
        corner_radii: Corners::default(),
        content_mask: whole_mask(),
        color,
        element_bounds: b,
        element_corner_radii: Corners::default(),
        inset: 0,
        pad: 0,
    }
}

/// A scene as painted: primitives, and layers of primitives.
#[derive(Clone)]
enum Op {
    Primitive(Primitive),
    Layer(Bounds<ScaledPixels>, Vec<Primitive>),
}

fn build(ops: &[Op]) -> Scene {
    let mut scene = Scene::default();
    for op in ops {
        match op {
            Op::Primitive(primitive) => scene.insert_primitive(primitive.clone()),
            Op::Layer(b, primitives) => {
                scene.push_layer(*b);
                for primitive in primitives {
                    scene.insert_primitive(primitive.clone());
                }
                scene.pop_layer();
            }
        }
    }
    scene.finish();
    scene
}

/// Builds the two scenes, the first one a window's first, and works out the
/// second's damage against it.
fn damage_between(prev_ops: &[Op], next_ops: &[Op]) -> Scene {
    let mut state = DamageState::default();
    let mut prev = build(prev_ops);
    diff_scenes(&Scene::default(), &mut prev, canvas_size(), &mut state);
    assert_eq!(prev.damage.since, 0);
    let mut next = build(next_ops);
    diff_scenes(&prev, &mut next, canvas_size(), &mut state);
    assert_eq!(next.damage.frame, 2);
    assert_eq!(next.damage.since, 1);
    next
}

const RED: Hsla = hsla(0., 1., 0.5, 1.);
const BLUE: Hsla = hsla(0.6, 1., 0.5, 1.);
const GREEN: Hsla = hsla(0.3, 1., 0.5, 0.5);

#[test]
fn identical_scenes_damage_nothing() {
    let ops = vec![
        Op::Primitive(quad(bounds(1., 1., 20., 20.), RED)),
        Op::Primitive(quad(bounds(10., 10., 20., 20.), BLUE)),
        Op::Layer(
            bounds(0., 40., 60., 10.),
            vec![
                Primitive::MonochromeSprite(mono(bounds(0., 40., 6., 9.), RED, 1)),
                Primitive::MonochromeSprite(mono(bounds(5., 40., 6., 9.), RED, 2)),
            ],
        ),
    ];
    let next = damage_between(&ops, &ops);
    assert!(next.damage.rects.is_empty());
    assert_eq!(next.damage.changed_primitives, 0);
}

#[test]
fn a_quad_changing_colour_damages_its_pixels() {
    let mut ops = vec![
        Op::Primitive(quad(bounds(1., 1., 20., 20.), RED)),
        Op::Primitive(quad(bounds(60.5, 60.25, 10., 10.), RED)),
    ];
    let prev = ops.clone();
    ops[1] = Op::Primitive(quad(bounds(60.5, 60.25, 10., 10.), BLUE));
    let next = damage_between(&prev, &ops);
    assert_eq!(next.damage.rects, vec![device(60, 60, 11, 11)]);
    assert_eq!(next.damage.changed_primitives, 2);
}

#[test]
fn an_inserted_primitive_damages_only_itself() {
    let row = |count: usize| -> Vec<Op> {
        (0..count)
            .map(|i| Op::Primitive(quad(bounds(i as f32 * 6., 100., 4., 4.), RED)))
            .collect()
    };
    let prev = row(20);
    let mut next_ops = vec![Op::Primitive(quad(bounds(2., 2., 4., 4.), BLUE))];
    next_ops.extend(row(20));
    let next = damage_between(&prev, &next_ops);
    assert_eq!(next.damage.rects, vec![device(2, 2, 4, 4)]);
    assert_eq!(next.damage.changed_primitives, 1);

    // And in the middle of the vectors, among primitives of its order.
    let mut next_ops = row(20);
    next_ops.insert(10, Op::Primitive(quad(bounds(30., 30., 4., 4.), BLUE)));
    let next = damage_between(&prev, &next_ops);
    assert_eq!(next.damage.rects, vec![device(30, 30, 4, 4)]);
}

#[test]
fn overlapping_primitives_of_a_layer_swapping_paint_order_are_damaged() {
    let a = quad(bounds(10., 10., 10., 10.), RED);
    let b = quad(bounds(15., 10., 10., 10.), BLUE);
    let far = quad(bounds(100., 100., 4., 4.), GREEN);
    let layer = bounds(0., 0., 128., 128.);
    let prev = vec![Op::Layer(layer, vec![a.clone(), far.clone(), b.clone()])];
    let next_ops = vec![Op::Layer(layer, vec![b, far, a])];
    let next = damage_between(&prev, &next_ops);
    assert_eq!(next.damage.rects.len(), 1);
    let rect = next.damage.rects[0];
    // Where the two overlap, the other one is on top now; `far` overlaps
    // neither, so its pixels do not depend on its place among them.
    assert!(rect.contains(&point(DevicePixels(15), DevicePixels(10))));
    assert!(rect.contains(&point(DevicePixels(19), DevicePixels(19))));
    assert!(!rect.contains(&point(DevicePixels(100), DevicePixels(100))));

    // Glyphs of one atlas tile in a layer draw in paint order too.
    let glyph = |x: f32, color| Primitive::MonochromeSprite(mono(bounds(x, 50., 6., 9.), color, 3));
    let prev = vec![Op::Layer(layer, vec![glyph(40., RED), glyph(43., BLUE)])];
    let next_ops = vec![Op::Layer(layer, vec![glyph(43., BLUE), glyph(40., RED)])];
    let next = damage_between(&prev, &next_ops);
    assert_eq!(next.damage.rects.len(), 1);
    let rect = next.damage.rects[0];
    assert!(rect.contains(&point(DevicePixels(43), DevicePixels(50))));
    assert!(rect.contains(&point(DevicePixels(45), DevicePixels(58))));
}

/// A path at `x`, 20 px square, as a triangle fan of four corners.
fn square_path(x: f32, color: Hsla) -> Primitive {
    let mut path = Path::new(point(px(x), px(10.)));
    path.line_to(point(px(x + 20.), px(10.)));
    path.line_to(point(px(x + 20.), px(30.)));
    path.line_to(point(px(x), px(30.)));
    path.content_mask = ContentMask {
        bounds: Bounds {
            origin: point(px(-50.), px(-50.)),
            size: size(px(300.), px(300.)),
        },
    };
    path.color = color.into();
    Primitive::Path(path.scale(1.))
}

#[test]
fn overlapping_paths_split_into_two_batches_are_damaged() {
    // Two overlapping paths are one batch. A quad drawn far away, at an order
    // between theirs, splits the batch: the paths' overlap blends otherwise.
    let paths = [
        Op::Primitive(square_path(10., RED)),
        Op::Primitive(square_path(20., BLUE)),
        Op::Primitive(quad(bounds(90., 90., 10., 10.), GREEN)),
    ];
    let mut split = paths.to_vec();
    split.push(Op::Primitive(quad(bounds(95., 95., 10., 10.), RED)));
    let next = damage_between(&paths, &split);
    let rects = &next.damage.rects;
    let covers = |x: i32, y: i32| {
        rects.iter().any(|r| {
            r.origin.x.0 <= x
                && x < r.origin.x.0 + r.size.width.0
                && r.origin.y.0 <= y
                && y < r.origin.y.0 + r.size.height.0
        })
    };
    assert!(covers(25, 20), "the paths' overlap is damaged: {rects:?}");
    // The quad, and the two paths of the batch it splits.
    assert_eq!(next.damage.changed_primitives, 3);

    // Without the split, the quad alone is damaged.
    let mut far = paths.to_vec();
    far.push(Op::Primitive(quad(bounds(110., 60., 10., 10.), RED)));
    let next = damage_between(&paths, &far);
    assert_eq!(next.damage.rects, vec![device(110, 60, 10, 10)]);
}

#[test]
fn a_shadow_damages_its_blur() {
    let drop = |color| {
        Op::Primitive(Primitive::Shadow(shadow(
            bounds(20., 20., 10., 10.),
            2.,
            color,
        )))
    };
    let next = damage_between(&[drop(RED)], &[drop(BLUE)]);
    assert_eq!(next.damage.rects, vec![device(14, 14, 22, 22)]);

    let inset = |color| {
        let mut shadow = shadow(bounds(22., 22., 6., 6.), 2., color);
        shadow.inset = 1;
        shadow.element_bounds = bounds(20., 20., 10., 10.);
        Op::Primitive(Primitive::Shadow(shadow))
    };
    let next = damage_between(&[inset(RED)], &[inset(BLUE)]);
    assert_eq!(next.damage.rects, vec![device(20, 20, 10, 10)]);
}

#[test]
fn a_transformed_sprite_damages_its_transformed_bounds() {
    let sprite = |color| {
        let mut sprite = mono(bounds(10., 10., 20., 10.), color, 1);
        // A quarter turn about the origin, then moved right: (x, y) is drawn
        // at (100 - y, x).
        sprite.transformation = TransformationMatrix {
            rotation_scale: [[0., -1.], [1., 0.]],
            translation: [100., 0.],
        };
        Op::Primitive(Primitive::MonochromeSprite(sprite))
    };
    let next = damage_between(&[sprite(RED)], &[sprite(BLUE)]);
    assert_eq!(next.damage.rects, vec![device(80, 10, 10, 20)]);
}

#[test]
fn a_dirty_scroll_layer_tile_is_damaged() {
    let key = LayerKey(3);
    let tile_sprite = |x: i32| PolychromeSprite {
        order: 0,
        pad: 0,
        grayscale: PaddedBool32::from(false),
        opacity: 1.,
        bounds: bounds(x as f32 * 32., 0., 32., 32.),
        content_mask: whole_mask(),
        corner_radii: Corners::default(),
        tile: AtlasTile {
            texture_id: layer_tile_texture_id(key),
            tile_id: layer_tile_id(TileCoord { x, y: 0 }),
            padding: 0,
            bounds: device(0, 0, 32, 32),
        },
    };
    let ops = vec![Op::Layer(
        bounds(0., 0., 128., 32.),
        vec![
            Primitive::PolychromeSprite(tile_sprite(0)),
            Primitive::PolychromeSprite(tile_sprite(1)),
            Primitive::PolychromeSprite(tile_sprite(2)),
        ],
    )];
    let frame = |generation, dirty: Vec<i32>| LayerFrame {
        key,
        generation,
        background: crate::Rgba::default(),
        tile_size: 32,
        content: LayerContent::default(),
        dirty_tiles: dirty.into_iter().map(|x| TileCoord { x, y: 0 }).collect(),
    };
    let diff = |prev_frame: Option<LayerFrame>, next_frame: LayerFrame| {
        let mut state = DamageState::default();
        let mut prev = build(&ops);
        prev.layers.frames.extend(prev_frame);
        diff_scenes(&Scene::default(), &mut prev, canvas_size(), &mut state);
        let mut next = build(&ops);
        next.layers.frames.push(next_frame);
        diff_scenes(&prev, &mut next, canvas_size(), &mut state);
        next.damage.rects
    };

    // The same generation: nothing changed, whatever it lists.
    assert_eq!(diff(Some(frame(4, vec![1])), frame(4, vec![1])), vec![]);
    // The next generation: its dirty tiles.
    assert_eq!(
        diff(Some(frame(4, vec![0])), frame(5, vec![1])),
        vec![device(32, 0, 32, 32)]
    );
    // A generation skipped, or a layer not composited before: every tile.
    assert_eq!(
        diff(Some(frame(3, vec![0])), frame(5, vec![1])),
        vec![device(0, 0, 96, 32)]
    );
    assert_eq!(diff(None, frame(5, vec![1])), vec![device(0, 0, 96, 32)]);
}

#[test]
fn damage_over_half_the_window_covers_it_and_a_resize_starts_over() {
    let prev = vec![Op::Primitive(quad(bounds(0., 0., 100., 100.), RED))];
    let next_ops = vec![Op::Primitive(quad(bounds(0., 0., 100., 100.), BLUE))];
    let next = damage_between(&prev, &next_ops);
    assert_eq!(next.damage.rects, vec![device(0, 0, CANVAS, CANVAS)]);
    assert!(!next.damage.is_full());

    let mut state = DamageState::default();
    let mut first = build(&prev);
    diff_scenes(&Scene::default(), &mut first, canvas_size(), &mut state);
    let mut second = build(&prev);
    diff_scenes(
        &first,
        &mut second,
        size(DevicePixels(64), DevicePixels(64)),
        &mut state,
    );
    assert!(second.damage.is_full());
    assert_eq!(second.damage.rects, vec![device(0, 0, 64, 64)]);
}

#[test]
fn nearby_rects_merge_and_too_many_merge_cheapest_first() {
    let row = |colors: &[Hsla]| -> Vec<Op> {
        colors
            .iter()
            .enumerate()
            .map(|(i, color)| Op::Primitive(quad(bounds(i as f32 * 7., 0., 2., 2.), *color)))
            .collect()
    };
    // 2px quads 5px apart merge into one rectangle.
    let next = damage_between(&row(&[RED, RED, RED]), &row(&[BLUE, BLUE, BLUE]));
    assert_eq!(next.damage.rects, vec![device(0, 0, 16, 2)]);

    // Seventeen far apart: two of them merge, the pair whose union adds the
    // fewest pixels (two in a row, 20px apart), and the rest stay apart.
    let spots = |color| -> Vec<Op> {
        (0..17)
            .map(|i| {
                let (x, y) = ((i % 6) as f32 * 20., (i / 6) as f32 * 20.);
                Op::Primitive(quad(bounds(x, y, 1., 1.), color))
            })
            .collect()
    };
    let next = damage_between(&spots(RED), &spots(BLUE));
    let rects = &next.damage.rects;
    assert_eq!(rects.len(), 16, "{rects:?}");
    assert_eq!(
        rects
            .iter()
            .filter(|r| r.size == size(DevicePixels(21), DevicePixels(1)))
            .count(),
        1,
        "{rects:?}"
    );
    assert_eq!(next.damage.area(), 15 + 21);
}

// Random scenes and random changes to them.

const PALETTE: [Hsla; 3] = [RED, BLUE, GREEN];

fn random_bounds(rng: &mut StdRng, max: f32) -> Bounds<ScaledPixels> {
    let snap = |value: f32, rng: &mut StdRng| {
        if rng.random_bool(0.5) {
            value.round()
        } else {
            value
        }
    };
    let x = snap(rng.random_range(-10.0..CANVAS as f32), rng);
    let y = snap(rng.random_range(-10.0..CANVAS as f32), rng);
    let w = snap(rng.random_range(0.5..max), rng);
    let h = snap(rng.random_range(0.5..max), rng);
    bounds(x, y, w, h)
}

fn random_mask(rng: &mut StdRng) -> ContentMask<ScaledPixels> {
    if rng.random_bool(0.7) {
        whole_mask()
    } else {
        ContentMask {
            bounds: random_bounds(rng, 60.),
        }
    }
}

fn random_primitive(rng: &mut StdRng) -> Primitive {
    let b = random_bounds(rng, 24.);
    let content_mask = random_mask(rng);
    let color = PALETTE[rng.random_range(0..PALETTE.len())];
    let tile_id = rng.random_range(0..3);
    match rng.random_range(0..8) {
        0 | 1 => Primitive::Quad(Quad {
            bounds: b,
            content_mask,
            background: color.into(),
            border_widths: Edges::all(ScaledPixels(1.)),
            ..Default::default()
        }),
        2 => {
            let mut shadow = shadow(b, rng.random_range(0.0..4.), color);
            shadow.content_mask = content_mask;
            if rng.random_bool(0.3) {
                shadow.inset = 1;
                shadow.element_bounds = random_bounds(rng, 30.);
            }
            Primitive::Shadow(shadow)
        }
        3 => Primitive::Underline(Underline {
            order: 0,
            pad: 0,
            bounds: b,
            content_mask,
            color,
            thickness: ScaledPixels(1.),
            wavy: PaddedBool32::from(rng.random_bool(0.5)),
        }),
        4 => {
            let mut sprite = mono(b, color, tile_id);
            sprite.content_mask = content_mask;
            if rng.random_bool(0.3) {
                let angle = rng.random_range(0.0..6.3f32);
                sprite.transformation = TransformationMatrix {
                    rotation_scale: [[angle.cos(), -angle.sin()], [angle.sin(), angle.cos()]],
                    translation: [rng.random_range(-20.0..20.), rng.random_range(-20.0..20.)],
                };
            }
            Primitive::MonochromeSprite(sprite)
        }
        5 => Primitive::SubpixelSprite(SubpixelSprite {
            order: 0,
            pad: 0,
            bounds: b,
            content_mask,
            color,
            tile: tile(1, tile_id, AtlasTextureKind::Subpixel),
            transformation: TransformationMatrix::unit(),
        }),
        6 => Primitive::PolychromeSprite(PolychromeSprite {
            order: 0,
            pad: 0,
            grayscale: PaddedBool32::from(false),
            opacity: color.a,
            bounds: b,
            content_mask,
            corner_radii: Corners::default(),
            tile: tile(
                rng.random_range(0..2),
                tile_id,
                AtlasTextureKind::Polychrome,
            ),
        }),
        _ => {
            let mut corner = |rng: &mut StdRng| {
                point(
                    px(b.origin.x.0 + rng.random_range(0.0..b.size.width.0)),
                    px(b.origin.y.0 + rng.random_range(0.0..b.size.height.0)),
                )
            };
            let mut path = Path::new(corner(rng));
            path.line_to(corner(rng));
            path.line_to(corner(rng));
            path.line_to(corner(rng));
            path.content_mask = ContentMask {
                bounds: Bounds {
                    origin: point(
                        px(content_mask.bounds.origin.x.0),
                        px(content_mask.bounds.origin.y.0),
                    ),
                    size: size(
                        px(content_mask.bounds.size.width.0),
                        px(content_mask.bounds.size.height.0),
                    ),
                },
            };
            path.color = color.into();
            Primitive::Path(path.scale(1.))
        }
    }
}

/// A random scene; `scale` makes it larger, its layers longer.
fn random_ops(rng: &mut StdRng, scale: usize) -> Vec<Op> {
    (0..rng.random_range(1..30 * scale))
        .map(|_| {
            if rng.random_bool(0.2) {
                // A text line: primitives crowded together, overlapping.
                let at = (
                    rng.random_range(-10.0..CANVAS as f32),
                    rng.random_range(-10.0..CANVAS as f32),
                );
                Op::Layer(
                    random_bounds(rng, 60.),
                    (0..rng.random_range(1..8 * scale))
                        .map(|_| {
                            let mut primitive = random_primitive(rng);
                            let origin = primitive.bounds().origin;
                            let spread = 12. * scale as f32;
                            let delta = point(
                                ScaledPixels(at.0 + rng.random_range(0.0..spread) - origin.x.0),
                                ScaledPixels(at.1 + rng.random_range(0.0..4.) - origin.y.0),
                            );
                            move_primitive(&mut primitive, delta);
                            primitive
                        })
                        .collect(),
                )
            } else {
                Op::Primitive(random_primitive(rng))
            }
        })
        .collect()
}

fn recolor(primitive: &mut Primitive, color: Hsla) {
    match primitive {
        Primitive::Shadow(shadow) => shadow.color = color,
        Primitive::Quad(quad) => quad.background = Background::from(color),
        Primitive::Path(path) => path.color = color.into(),
        Primitive::Underline(underline) => underline.color = color,
        Primitive::MonochromeSprite(sprite) => sprite.color = color,
        Primitive::SubpixelSprite(sprite) => sprite.color = color,
        Primitive::PolychromeSprite(sprite) => sprite.opacity = color.a * 0.5,
        Primitive::Surface(_) => {}
    }
}

/// Every primitive's place: its op, and its index in a layer.
fn places(ops: &[Op]) -> Vec<(usize, Option<usize>)> {
    let mut places = Vec::new();
    for (index, op) in ops.iter().enumerate() {
        match op {
            Op::Primitive(_) => places.push((index, None)),
            Op::Layer(_, primitives) => {
                places.extend((0..primitives.len()).map(|inner| (index, Some(inner))))
            }
        }
    }
    places
}

fn primitive_at(ops: &mut [Op], place: (usize, Option<usize>)) -> &mut Primitive {
    match (&mut ops[place.0], place.1) {
        (Op::Primitive(primitive), None) => primitive,
        (Op::Layer(_, primitives), Some(inner)) => &mut primitives[inner],
        _ => unreachable!(),
    }
}

fn mutate(ops: &mut Vec<Op>, rng: &mut StdRng) {
    let places = places(ops);
    if places.is_empty() || ops.is_empty() {
        ops.push(Op::Primitive(random_primitive(rng)));
        return;
    }
    let place = places[rng.random_range(0..places.len())];
    match rng.random_range(0..8) {
        0 => recolor(
            primitive_at(ops, place),
            PALETTE[rng.random_range(0..PALETTE.len())],
        ),
        1 => move_primitive(
            primitive_at(ops, place),
            point(
                ScaledPixels(rng.random_range(-3..=3) as f32),
                ScaledPixels(rng.random_range(-3..=3) as f32),
            ),
        ),
        2 => {
            let primitive = random_primitive(rng);
            match &mut ops[place.0] {
                Op::Layer(_, primitives) if rng.random_bool(0.5) => {
                    let at = rng.random_range(0..=primitives.len());
                    primitives.insert(at, primitive)
                }
                _ => ops.insert(rng.random_range(0..=ops.len()), Op::Primitive(primitive)),
            }
        }
        3 => match (&mut ops[place.0], place.1) {
            (Op::Layer(_, primitives), Some(inner)) if primitives.len() > 1 => {
                primitives.remove(inner);
            }
            _ => {
                ops.remove(place.0);
            }
        },
        4 => {
            let op = ops.remove(place.0);
            let at = rng.random_range(0..=ops.len());
            ops.insert(at, op);
        }
        5 => {
            if let Op::Layer(_, primitives) = &mut ops[place.0] {
                let len = primitives.len();
                primitives.swap(rng.random_range(0..len), rng.random_range(0..len));
            }
        }
        6 => {
            if let Op::Layer(b, _) = &mut ops[place.0] {
                *b = random_bounds(rng, 60.);
            }
        }
        _ => {
            // The same primitive painted again on top, as a hover can.
            let primitive = primitive_at(ops, place).clone();
            ops.push(Op::Primitive(primitive));
        }
    }
}

/// A primitive as the pixels it covers and what it draws there.
struct Drawn {
    /// Drawing order: order, kind, index in the kind's vector.
    key: (u32, u8, usize),
    pixels: (i32, i32, i32, i32),
    value: String,
}

fn drawn(scene: &Scene) -> Vec<Drawn> {
    let mut all = Vec::new();
    let mut add = |primitive: Primitive, kind: u8, index: usize, order: u32, value: String| {
        let visible = visible_bounds(&primitive);
        let pad = if kind == 2 { 1 } else { 0 };
        let x0 = visible.origin.x.0;
        let y0 = visible.origin.y.0;
        let x1 = x0 + visible.size.width.0;
        let y1 = y0 + visible.size.height.0;
        if !(x1 > x0 && y1 > y0) {
            return;
        }
        all.push(Drawn {
            key: (order, kind, index),
            pixels: (
                x0.floor() as i32 - pad,
                y0.floor() as i32 - pad,
                x1.ceil() as i32 + pad,
                y1.ceil() as i32 + pad,
            ),
            value,
        });
    };
    for (i, p) in scene.shadows.iter().enumerate() {
        add(Primitive::Shadow(*p), 0, i, p.order, format!("{p:?}"));
    }
    for (i, p) in scene.quads.iter().enumerate() {
        add(
            Primitive::Quad(*p),
            1,
            i,
            p.order,
            format!("{p:?} {:?}", p.background.solid),
        );
    }
    for (i, p) in scene.paths.iter().enumerate() {
        let value = format!(
            "{} {:?} {:?} {:?} {:?}",
            p.order, p.bounds, p.content_mask, p.color, p.vertices
        );
        add(Primitive::Path(p.clone()), 2, i, p.order, value);
    }
    for (i, p) in scene.underlines.iter().enumerate() {
        add(Primitive::Underline(*p), 3, i, p.order, format!("{p:?}"));
    }
    for (i, p) in scene.monochrome_sprites.iter().enumerate() {
        add(
            Primitive::MonochromeSprite(*p),
            4,
            i,
            p.order,
            format!("{p:?}"),
        );
    }
    for (i, p) in scene.subpixel_sprites.iter().enumerate() {
        add(
            Primitive::SubpixelSprite(*p),
            5,
            i,
            p.order,
            format!("{p:?}"),
        );
    }
    for (i, p) in scene.polychrome_sprites.iter().enumerate() {
        add(
            Primitive::PolychromeSprite(*p),
            6,
            i,
            p.order,
            format!("{p:?}"),
        );
    }
    all.sort_by_key(|drawn| drawn.key);
    all
}

/// What covers each pixel of the canvas, in drawing order.
fn covering(drawn: &[Drawn]) -> Vec<Vec<&str>> {
    let mut pixels = vec![Vec::new(); (CANVAS * CANVAS) as usize];
    for d in drawn {
        for y in d.pixels.1.max(0)..d.pixels.3.min(CANVAS) {
            for x in d.pixels.0.max(0)..d.pixels.2.min(CANVAS) {
                pixels[(y * CANVAS + x) as usize].push(d.value.as_str());
            }
        }
    }
    pixels
}

/// Changes random scenes at random, and checks that outside the damage,
/// every pixel is covered by the same primitives in the same order in both
/// scenes, and that small changes damage less than the window.
fn check_random_changes(seeds: std::ops::Range<u64>, scale: usize) {
    let mut small_changes = 0;
    let mut small_damage = 0;
    for seed in seeds {
        let mut rng = StdRng::seed_from_u64(seed);
        let prev_ops = random_ops(&mut rng, scale);
        let mut next_ops = prev_ops.clone();
        let mutations = rng.random_range(0..4);
        for _ in 0..mutations {
            mutate(&mut next_ops, &mut rng);
        }
        let next = damage_between(&prev_ops, &next_ops);
        let prev = build(&prev_ops);
        let (prev_drawn, next_drawn) = (drawn(&prev), drawn(&next));
        let (before, after) = (covering(&prev_drawn), covering(&next_drawn));
        let damage = &next.damage.rects;
        for y in 0..CANVAS {
            for x in 0..CANVAS {
                let pixel = point(DevicePixels(x), DevicePixels(y));
                if damage.iter().any(|rect| rect.contains(&pixel)) {
                    continue;
                }
                let at = (y * CANVAS + x) as usize;
                assert_eq!(
                    before[at], after[at],
                    "seed {seed}: pixel ({x}, {y}) is outside the damage {damage:?} \
                     but draws differently"
                );
            }
        }
        if mutations == 0 {
            assert!(damage.is_empty(), "seed {seed}: nothing changed");
        }
        if mutations == 1 {
            small_changes += 1;
            if next.damage.area() < (CANVAS * CANVAS / 2) as i64 {
                small_damage += 1;
            }
        }
    }
    assert!(
        small_damage * 3 > small_changes * 2,
        "only {small_damage} of {small_changes} single changes damaged less than the window"
    );
}

#[test]
fn damage_is_exact_for_random_changes() {
    check_random_changes(0..1000, 1);
}

/// Larger scenes, whose draw orders hold many primitives, which the diff
/// walks side by side rather than matching by value.
#[test]
fn damage_is_exact_for_random_changes_to_larger_scenes() {
    check_random_changes(0..300, 4);
}

// Windows.

struct Label {
    label: Rc<Cell<u32>>,
}

impl Render for Label {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(crate::white())
            .child(format!("cell {}", self.label.get()))
    }
}

struct Grid {
    cells: Vec<Entity<Label>>,
}

impl Render for Grid {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_wrap()
            .bg(crate::black())
            .children(self.cells.iter().map(|cell| {
                cell.clone()
                    .cached(StyleRefinement::default().w(px(100.)).h(px(20.)))
            }))
    }
}

fn grid_window(cx: &mut TestAppContext) -> (WindowHandle<Grid>, Vec<Rc<Cell<u32>>>) {
    let labels: Vec<_> = (0..60).map(|i| Rc::new(Cell::new(i))).collect();
    let window = cx.add_window({
        let labels = labels.clone();
        move |_, cx| Grid {
            cells: labels
                .into_iter()
                .map(|label| cx.new(|_| Label { label }))
                .collect(),
        }
    });
    (window, labels)
}

/// The damage of the scene the window drew last. Test windows draw when
/// an update leaves them dirty.
fn damage(cx: &mut TestAppContext, window: WindowHandle<Grid>) -> crate::SceneDamage {
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, _| {
        window.rendered_frame.scene.damage.clone()
    })
    .unwrap()
}

fn glyph_bounds(cx: &mut TestAppContext, window: WindowHandle<Grid>) -> Vec<Bounds<ScaledPixels>> {
    cx.update_window(window.into(), |_, window, _| {
        window
            .rendered_frame
            .scene
            .monochrome_sprites
            .iter()
            .map(|sprite| sprite.bounds)
            .collect()
    })
    .unwrap()
}

#[test]
fn a_windows_first_scene_is_damaged_whole() {
    let mut cx = TestAppContext::with_text_system(Arc::new(GlyphBoxTextSystem(NoopTextSystem)));
    let (window, _) = grid_window(&mut cx);
    let damage = damage(&mut cx, window);
    assert_eq!(damage.frame, 1);
    assert_eq!(damage.since, 0);
    assert!(damage.is_full());
    assert_eq!(damage.rects.len(), 1);
}

#[test]
fn a_window_redrawn_unchanged_damages_nothing() {
    let mut cx = TestAppContext::with_text_system(Arc::new(GlyphBoxTextSystem(NoopTextSystem)));
    let (window, _) = grid_window(&mut cx);
    let first = damage(&mut cx, window);
    window.update(&mut cx, |_, _, cx| cx.notify()).unwrap();
    let second = damage(&mut cx, window);
    assert_eq!(second.frame, first.frame + 1);
    assert_eq!(second.since, first.frame);
    assert_eq!(second.rects, vec![]);
    assert_eq!(second.changed_primitives, 0);
}

#[test]
fn a_small_view_changing_damages_little_of_the_window() {
    let mut cx = TestAppContext::with_text_system(Arc::new(GlyphBoxTextSystem(NoopTextSystem)));
    let (window, labels) = grid_window(&mut cx);
    let mut last = damage(&mut cx, window).frame;
    cx.update_window(window.into(), |_, window, _| window.reset_layout_stats())
        .unwrap();
    let cells = window
        .update(&mut cx, |grid, _, _| grid.cells.clone())
        .unwrap();
    let frames = 10;
    for frame in 0..frames {
        let index = 7 + frame * 3;
        let before = glyph_bounds(&mut cx, window);
        labels[index].set(100 + frame as u32);
        cells[index].update(&mut cx, |_, cx| cx.notify());
        let damage = damage(&mut cx, window);
        assert_eq!(damage.frame, last + 1);
        assert_eq!(damage.since, last);
        last = damage.frame;
        let after = glyph_bounds(&mut cx, window);
        let window_area = cx
            .update_window(window.into(), |_, window, _| {
                let size = window.viewport_size();
                let scale = window.scale_factor();
                (size.width.0 * scale).ceil() as i64 * (size.height.0 * scale).ceil() as i64
            })
            .unwrap();
        assert!(!damage.rects.is_empty(), "frame {frame}: the cell changed");
        assert!(
            damage.area() * 16 <= window_area,
            "frame {frame}: {:?} is more than a sixteenth of the window",
            damage.rects
        );
        // The glyphs that came and went are inside the damage.
        let changed: Vec<_> = after
            .iter()
            .filter(|glyph| !before.contains(glyph))
            .chain(before.iter().filter(|glyph| !after.contains(glyph)))
            .collect();
        assert!(!changed.is_empty(), "frame {frame}: the text changed");
        for glyph in changed {
            let center = point(
                DevicePixels((glyph.origin.x.0 + glyph.size.width.0 / 2.) as i32),
                DevicePixels((glyph.origin.y.0 + glyph.size.height.0 / 2.) as i32),
            );
            assert!(
                damage.rects.iter().any(|rect| rect.contains(&center)),
                "frame {frame}: {center:?} not in {:?}",
                damage.rects
            );
        }
    }
    let stats = cx
        .update_window(window.into(), |_, window, _| window.layout_stats())
        .unwrap();
    assert_eq!(stats.damage_frames, frames as u64);
    assert_eq!(stats.small_damage_frames, frames as u64);
    assert_eq!(stats.full_damage_frames, 0);
    assert!(stats.damaged_pixels * 16 <= stats.window_pixels);
    assert!(stats.changed_primitives > 0);
}
