//! Partial frames on Metal: a frame drawn only inside its damage, over the
//! canvas holding the scene the damage compares with, must come out byte for
//! byte as the same scene drawn whole; and the frames that cannot be drawn
//! partially are drawn whole. Without a Metal device the tests print a skip
//! and return.

use std::borrow::Cow;
use std::sync::Arc;

use gpui::{
    AtlasKey, AtlasTile, Bounds, ContentMask, Corners, DevicePixels, Edges, FontId, GlyphId, Hsla,
    ImageId, MonochromeSprite, Path, PlatformAtlas, PolychromeSprite, Quad, RenderGlyphParams,
    RenderImageParams, ScaledPixels, Scene, Shadow, Size, TransformationMatrix, point, px, rgba,
    size,
};
use parking_lot::Mutex;

use super::{Frame, FullReason, Plan, decide};
use crate::metal_renderer::{InstanceBufferPool, MetalRenderer, read_texture_to_image};

const WIDTH: i32 = 400;
const HEIGHT: i32 = 300;

#[test]
fn a_partial_frame_equals_the_scene_drawn_whole() {
    for opaque in [true, false] {
        let Some(mut harness) = Harness::new(device_size(WIDTH, HEIGHT), opaque) else {
            return;
        };
        let tiles = Tiles::new(&harness);
        harness.draw(&scene(&tiles, Change::None, 1, 0, &[]));
        assert_eq!(harness.last_plan(), Plan::Full(FullReason::NoCanvas));

        // The quad under the path, the glyph and the shadow change; the path
        // crosses the damage's edges unchanged.
        let damage = [rect(40, 40, 120, 80), rect(210, 138, 80, 70)];
        let partial = harness.draw(&scene(&tiles, Change::Some, 2, 1, &damage));
        assert_eq!(harness.last_plan(), Plan::Partial(damage.to_vec()));

        let whole = whole(&tiles, Change::Some, opaque);
        assert_same_pixels(&partial, &whole);
    }
}

#[test]
fn a_frame_with_empty_damage_shows_the_canvas() {
    let Some(mut harness) = Harness::new(device_size(WIDTH, HEIGHT), true) else {
        return;
    };
    let tiles = Tiles::new(&harness);
    let first = harness.draw(&scene(&tiles, Change::Some, 1, 0, &[]));
    let second = harness.draw(&scene(&tiles, Change::Some, 2, 1, &[]));
    assert_eq!(harness.last_plan(), Plan::Partial(Vec::new()));
    assert_same_pixels(&second, &first);
}

#[test]
fn consecutive_partial_frames_stay_exact() {
    let Some(mut harness) = Harness::new(device_size(WIDTH, HEIGHT), true) else {
        return;
    };
    let tiles = Tiles::new(&harness);
    let damage = [rect(40, 40, 120, 80), rect(210, 138, 80, 70)];
    harness.draw(&scene(&tiles, Change::None, 1, 0, &[]));
    harness.draw(&scene(&tiles, Change::Some, 2, 1, &damage));
    let back = harness.draw(&scene(&tiles, Change::None, 3, 2, &damage));
    assert_eq!(harness.last_plan(), Plan::Partial(damage.to_vec()));
    assert_same_pixels(&back, &whole(&tiles, Change::None, true));
}

#[test]
fn frames_that_cannot_be_partial_are_drawn_whole() {
    let Some(mut harness) = Harness::new(device_size(WIDTH, HEIGHT), true) else {
        return;
    };
    let tiles = Tiles::new(&harness);
    let damage = [rect(40, 40, 120, 80)];
    harness.draw(&scene(&tiles, Change::None, 1, 0, &[]));

    // Damage relative to a scene the canvas does not hold.
    let drawn = harness.draw(&scene(&tiles, Change::Some, 3, 2, &damage));
    assert_eq!(harness.last_plan(), Plan::Full(FullReason::NotComparable));
    assert_same_pixels(&drawn, &whole(&tiles, Change::Some, true));

    // A tile written to the atlas since the last frame.
    glyph_tile(&harness, 77);
    harness.draw(&scene(&tiles, Change::None, 4, 3, &damage));
    assert_eq!(harness.last_plan(), Plan::Full(FullReason::AtlasWritten));

    // A resized drawable.
    harness.resize(device_size(WIDTH + 10, HEIGHT));
    harness.draw(&scene(&tiles, Change::Some, 5, 4, &damage));
    assert_eq!(harness.last_plan(), Plan::Full(FullReason::Resized));

    // And partial again once the canvas holds the scene compared with.
    harness.draw(&scene(&tiles, Change::None, 6, 5, &damage));
    assert_eq!(harness.last_plan(), Plan::Partial(damage.to_vec()));

    // A window turned transparent: its canvas was cleared opaque.
    harness.renderer.update_transparency(true);
    harness.draw(&scene(&tiles, Change::Some, 7, 6, &damage));
    assert_eq!(harness.last_plan(), Plan::Full(FullReason::NoCanvas));
}

#[test]
fn decisions() {
    let window = device_size(100, 100);
    let damage = [rect(10, 10, 20, 20)];
    let frame = |canvas, number, since| Frame {
        canvas,
        size: window,
        last_drawn: 4,
        number,
        since,
        damage: &damage,
        atlas_written: false,
        has_surfaces: false,
    };
    assert_eq!(decide(&frame(None, 5, 4)), Plan::Full(FullReason::NoCanvas));
    assert_eq!(
        decide(&frame(Some(device_size(50, 100)), 5, 4)),
        Plan::Full(FullReason::Resized)
    );
    for (number, since) in [(0, 4), (5, 0), (5, 3)] {
        assert_eq!(
            decide(&frame(Some(window), number, since)),
            Plan::Full(FullReason::NotComparable)
        );
    }
    assert_eq!(
        decide(&Frame {
            atlas_written: true,
            ..frame(Some(window), 5, 4)
        }),
        Plan::Full(FullReason::AtlasWritten)
    );
    assert_eq!(
        decide(&Frame {
            has_surfaces: true,
            ..frame(Some(window), 5, 4)
        }),
        Plan::Full(FullReason::Surfaces)
    );
    assert_eq!(
        decide(&frame(Some(window), 5, 4)),
        Plan::Partial(damage.to_vec())
    );

    // The scene the canvas holds, drawn again.
    assert_eq!(
        decide(&frame(Some(window), 4, 3)),
        Plan::Partial(Vec::new())
    );
    assert_eq!(
        decide(&Frame {
            atlas_written: true,
            ..frame(Some(window), 4, 3)
        }),
        Plan::Full(FullReason::AtlasWritten)
    );

    // Clamped to the window, empty rectangles dropped.
    let outside = [rect(90, 90, 20, 20), rect(200, 200, 5, 5)];
    assert_eq!(
        decide(&Frame {
            damage: &outside,
            ..frame(Some(window), 5, 4)
        }),
        Plan::Partial(vec![rect(90, 90, 10, 10)])
    );
    // Over half the window.
    let large = [rect(0, 0, 100, 51)];
    assert_eq!(
        decide(&Frame {
            damage: &large,
            ..frame(Some(window), 5, 4)
        }),
        Plan::Full(FullReason::LargeDamage)
    );
}

// --- scenes --- //

#[derive(Clone, Copy, PartialEq)]
enum Change {
    None,
    Some,
}

/// The atlas tiles the scenes draw, uploaded before the first frame.
struct Tiles {
    glyph: AtlasTile,
    image: AtlasTile,
}

impl Tiles {
    fn new(harness: &Harness) -> Self {
        Tiles {
            glyph: glyph_tile(harness, 1),
            image: image_tile(harness),
        }
    }
}

/// A window's scene, numbered `number` with damage since `since`. With
/// `Change::Some`, it differs inside (40, 40, 120, 80) and (220, 150, 60, 50)
/// only.
fn scene(
    tiles: &Tiles,
    change: Change,
    number: u64,
    since: u64,
    damage: &[Bounds<DevicePixels>],
) -> Scene {
    let changed = change == Change::Some;
    let mut scene = Scene::default();
    scene.insert_primitive(quad(sp(0., 0., 400., 300.), rgba(0xf4f4f0ff).into()));
    scene.insert_primitive(Quad {
        bounds: sp(30.5, 30., 340., 240.),
        content_mask: no_mask(),
        background: Hsla::from(rgba(0xffffffff)).into(),
        border_color: rgba(0x333333ff).into(),
        corner_radii: Corners::all(ScaledPixels(10.)),
        border_widths: Edges::all(ScaledPixels(2.)),
        ..Default::default()
    });
    // Inside the first damage rectangle: a quad whose color changes.
    let color = if changed { 0x2266ccff } else { 0xcc3322ff };
    scene.insert_primitive(quad(sp(50., 50., 100., 60.), rgba(color).into()));
    // A shadow whose blur reaches into the second rectangle, and changes.
    scene.insert_primitive(Shadow {
        order: 0,
        blur_radius: ScaledPixels(if changed { 6. } else { 3. }),
        bounds: sp(232., 160., 30., 25.),
        corner_radii: Corners::all(ScaledPixels(4.)),
        content_mask: no_mask(),
        color: rgba(0x00000080).into(),
        element_bounds: sp(232., 160., 30., 25.),
        element_corner_radii: Corners::all(ScaledPixels(4.)),
        inset: 0,
        pad: 0,
    });
    // A glyph that moves within the first rectangle.
    scene.insert_primitive(MonochromeSprite {
        order: 0,
        pad: 0,
        bounds: sp(if changed { 120. } else { 60. }, 90., 16., 16.),
        content_mask: no_mask(),
        color: rgba(0x111111ff).into(),
        tile: tiles.glyph,
        transformation: TransformationMatrix::unit(),
    });
    // An image outside the damage.
    scene.insert_primitive(PolychromeSprite {
        order: 0,
        pad: 0,
        grayscale: false.into(),
        opacity: 1.,
        bounds: sp(300., 60., 20., 20.),
        content_mask: no_mask(),
        corner_radii: Corners::all(ScaledPixels(4.)),
        tile: tiles.image,
    });
    // A path that crosses both rectangles' edges without changing.
    let mut path = Path::new(point(px(20.), px(100.)));
    path.line_to(point(px(300.5), px(80.)));
    path.curve_to(point(px(260.), px(230.)), point(px(200.), px(120.)));
    path.line_to(point(px(20.), px(100.)));
    path.content_mask = ContentMask {
        bounds: no_mask().bounds.map(|c| px(c.0)),
    };
    path.color = Hsla::from(rgba(0x8800ffaa)).into();
    scene.insert_primitive(path.scale(1.));
    scene.finish();
    scene.damage.frame = number;
    scene.damage.since = since;
    scene.damage.rects = damage.to_vec();
    scene
}

/// The scene with `change`, drawn whole on a renderer of its own.
fn whole(tiles: &Tiles, change: Change, opaque: bool) -> Vec<u8> {
    let mut harness = Harness::new(device_size(WIDTH, HEIGHT), opaque).expect("a Metal device");
    // The fresh atlas holds the same tiles at the same places, uploaded in
    // the same order.
    let fresh = Tiles::new(&harness);
    assert_eq!(fresh.glyph, tiles.glyph);
    assert_eq!(fresh.image, tiles.image);
    let drawn = harness.draw(&scene(&fresh, change, 1, 0, &[]));
    assert_eq!(harness.last_plan(), Plan::Full(FullReason::NoCanvas));
    drawn
}

// --- harness --- //

/// A headless renderer drawing window frames, through the canvas, into a
/// texture standing for the drawable, read back after each frame.
struct Harness {
    renderer: MetalRenderer,
    target: metal::Texture,
    shared: bool,
    size: Size<DevicePixels>,
}

impl Harness {
    fn new(size: Size<DevicePixels>, opaque: bool) -> Option<Harness> {
        if metal::Device::system_default().is_none() && metal::Device::all().is_empty() {
            eprintln!("skipped: no Metal device");
            return None;
        }
        let pool = Arc::new(Mutex::new(InstanceBufferPool::default()));
        let mut renderer = MetalRenderer::new_headless(pool);
        renderer.opaque = opaque;
        let shared =
            cfg!(target_os = "ios") || renderer.device.supports_family(metal::MTLGPUFamily::Apple1);
        let target = new_target(&renderer.device, size, shared);
        renderer.update_drawable_size(size);
        Some(Harness {
            renderer,
            target,
            shared,
            size,
        })
    }

    fn resize(&mut self, size: Size<DevicePixels>) {
        self.size = size;
        self.target = new_target(&self.renderer.device, size, self.shared);
        self.renderer.update_drawable_size(size);
    }

    /// Draws `scene` as a window frame and returns the target's RGBA bytes.
    fn draw(&mut self, scene: &Scene) -> Vec<u8> {
        objc2::rc::autoreleasepool(|_| {
            let command_buffer =
                super::render_frame(&mut self.renderer, scene, &self.target, self.size)
                    .expect("frame rendered");
            if !self.shared {
                let blit = command_buffer.new_blit_command_encoder();
                blit.synchronize_resource(&self.target);
                blit.end_encoding();
            }
            command_buffer.commit();
            command_buffer.wait_until_completed();
            read_texture_to_image(&self.target)
                .expect("target read back")
                .into_raw()
        })
    }

    fn last_plan(&self) -> Plan {
        self.renderer
            .fast_partial
            .last_plan
            .clone()
            .expect("a frame was drawn")
    }
}

fn new_target(device: &metal::DeviceRef, size: Size<DevicePixels>, shared: bool) -> metal::Texture {
    let descriptor = metal::TextureDescriptor::new();
    descriptor.set_width(size.width.0 as u64);
    descriptor.set_height(size.height.0 as u64);
    descriptor.set_pixel_format(metal::MTLPixelFormat::BGRA8Unorm);
    descriptor.set_usage(metal::MTLTextureUsage::RenderTarget | metal::MTLTextureUsage::ShaderRead);
    descriptor.set_storage_mode(if shared {
        metal::MTLStorageMode::Shared
    } else {
        metal::MTLStorageMode::Managed
    });
    device.new_texture(&descriptor)
}

/// A 16×16 monochrome glyph of a made-up font, uploaded to the atlas.
fn glyph_tile(harness: &Harness, glyph: u32) -> AtlasTile {
    let key = AtlasKey::Glyph(RenderGlyphParams {
        font_id: FontId(9_998),
        glyph_id: GlyphId(glyph),
        font_size: px(12.),
        subpixel_variant: point(0, 0),
        scale_factor: 1.,
        is_emoji: false,
        subpixel_rendering: false,
        dilation: 0,
    });
    let bytes: Vec<u8> = (0..16 * 16).map(|i| ((i * 37) % 256) as u8).collect();
    harness
        .renderer
        .sprite_atlas()
        .get_or_insert_with(key, &mut || {
            Ok(Some((device_size(16, 16), Cow::Owned(bytes.clone()))))
        })
        .expect("glyph uploaded")
        .expect("glyph tile")
}

/// A 20×20 image uploaded to the atlas.
fn image_tile(harness: &Harness) -> AtlasTile {
    let key = AtlasKey::Image(RenderImageParams {
        image_id: ImageId(9_998),
        frame_index: 0,
    });
    let bytes: Vec<u8> = (0..20 * 20)
        .flat_map(|i: u32| {
            [
                (i * 7 % 256) as u8,
                (i * 13 % 256) as u8,
                (i * 3 % 256) as u8,
                255,
            ]
        })
        .collect();
    harness
        .renderer
        .sprite_atlas()
        .get_or_insert_with(key, &mut || {
            Ok(Some((device_size(20, 20), Cow::Owned(bytes.clone()))))
        })
        .expect("image uploaded")
        .expect("image tile")
}

fn assert_same_pixels(actual: &[u8], expected: &[u8]) {
    assert_eq!(actual.len(), expected.len());
    let width = WIDTH as usize;
    let differing: Vec<usize> = (0..actual.len() / 4)
        .filter(|i| actual[i * 4..i * 4 + 4] != expected[i * 4..i * 4 + 4])
        .collect();
    if let Some(&first) = differing.first() {
        panic!(
            "{} pixels differ; first at ({}, {}): {:?} instead of {:?}",
            differing.len(),
            first % width,
            first / width,
            &actual[first * 4..first * 4 + 4],
            &expected[first * 4..first * 4 + 4],
        );
    }
}

fn rect(x: i32, y: i32, width: i32, height: i32) -> Bounds<DevicePixels> {
    Bounds {
        origin: point(DevicePixels(x), DevicePixels(y)),
        size: device_size(width, height),
    }
}

fn sp(x: f32, y: f32, w: f32, h: f32) -> Bounds<ScaledPixels> {
    Bounds {
        origin: point(ScaledPixels(x), ScaledPixels(y)),
        size: size(ScaledPixels(w), ScaledPixels(h)),
    }
}

fn no_mask() -> ContentMask<ScaledPixels> {
    ContentMask {
        bounds: sp(-10_000., -10_000., 20_000., 20_000.),
    }
}

fn quad(bounds: Bounds<ScaledPixels>, color: Hsla) -> Quad {
    Quad {
        bounds,
        content_mask: no_mask(),
        background: color.into(),
        ..Default::default()
    }
}

fn device_size(width: i32, height: i32) -> Size<DevicePixels> {
    size(DevicePixels(width), DevicePixels(height))
}
