//! Paths drawn as path tiles, among quads, in a fast frame.

use gpui::{
    Bounds, ContentMask, DevicePixels, Hsla, PaintedPath, PathBuilder, PlatformHeadlessRenderer as _, Quad,
    ScaledPixels, Scene, point, px, size,
};

use crate::WgpuHeadlessRenderer;

const RED: u32 = 0xff0000ff;
const BLUE: u32 = 0x0000ffff;

fn color(rgba: u32) -> Hsla {
    gpui::rgba(rgba).into()
}

fn bounds(x: f32, y: f32, width: f32, height: f32) -> Bounds<ScaledPixels> {
    Bounds {
        origin: point(ScaledPixels(x), ScaledPixels(y)),
        size: size(ScaledPixels(width), ScaledPixels(height)),
    }
}

fn everywhere() -> ContentMask<ScaledPixels> {
    ContentMask {
        bounds: bounds(0., 0., 4096., 4096.),
    }
}

fn rectangle_path(x: f32, y: f32, width: f32, height: f32, rgba: u32) -> PaintedPath {
    let mut path = PathBuilder::fill();
    path.move_to(point(px(x), px(y)));
    path.line_to(point(px(x + width), px(y)));
    path.line_to(point(px(x + width), px(y + height)));
    path.line_to(point(px(x), px(y + height)));
    path.close();
    PaintedPath::new(
        path.build().expect("rectangle path"),
        1.,
        ContentMask {
            bounds: Bounds {
                origin: point(px(0.), px(0.)),
                size: size(px(4096.), px(4096.)),
            },
        },
        color(rgba).into(),
    )
}

fn rectangle_quad(x: f32, y: f32, width: f32, height: f32, rgba: u32) -> Quad {
    Quad {
        bounds: bounds(x, y, width, height),
        content_mask: everywhere(),
        background: color(rgba).into(),
        ..Default::default()
    }
}

/// Two overlapping rectangles at `(x, y)`, drawn as paths or as quads, with a
/// quad over the first drawn before the second, so that as paths each is
/// rasterized in a pass of its own.
fn scene(x: f32, y: f32, as_paths: bool) -> Scene {
    let mut scene = Scene::default();
    let rectangle = |scene: &mut Scene, x, y, rgba| {
        if as_paths {
            scene.insert_primitive(rectangle_path(x, y, 24., 16., rgba));
        } else {
            scene.insert_primitive(rectangle_quad(x, y, 24., 16., rgba));
        }
    };
    rectangle(&mut scene, x, y, RED);
    scene.insert_primitive(rectangle_quad(x + 4., y + 4., 8., 8., 0x00ff00ff));
    rectangle(&mut scene, x + 12., y + 8., BLUE);
    scene.finish();
    scene
}

fn assert_same_pixels(paths: &image::RgbaImage, quads: &image::RgbaImage, frame: usize) {
    assert_eq!(paths.dimensions(), quads.dimensions());
    for (x, y, path) in paths.enumerate_pixels() {
        let quad = quads.get_pixel(x, y);
        assert_eq!(
            path.0, quad.0,
            "frame {frame}, pixel ({x}, {y}): drawn as a path, as a quad"
        );
    }
}

/// Rectangles on whole pixels cover every sample of a pixel or none, so drawn
/// as paths through the multisampled pass they must come out exactly as quads
/// do: what the resolve writes is all the frame needs of the samples. Each
/// path pass starts from a cleared attachment, so the second frame, drawn
/// elsewhere, shows nothing of the first.
/// Rectangles on whole pixels cover every pixel fully or not at all, so drawn
/// as path tiles they must come out exactly as quads do, wherever the frame
/// puts them.
#[test]
fn paths_on_whole_pixels_draw_as_quads_do() {
    let Ok(mut renderer) = WgpuHeadlessRenderer::new() else {
        eprintln!("skipped: no wgpu adapter");
        return;
    };
    let target = size(DevicePixels(64), DevicePixels(64));
    for (frame, (x, y)) in [(4., 4.), (20., 30.)].into_iter().enumerate() {
        let paths = renderer
            .render_scene_to_image(&scene(x, y, true), target)
            .unwrap();
        let quads = renderer
            .render_scene_to_image(&scene(x, y, false), target)
            .unwrap();
        assert_same_pixels(&paths, &quads, frame);
    }
}
