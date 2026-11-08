//! Paths drawn through the multisampled path pass, which discards its samples
//! once they are resolved (see `path_store_op`).

use gpui::{
    Bounds, ContentMask, DevicePixels, Hsla, Path, PlatformHeadlessRenderer as _, Quad,
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

fn rectangle_path(x: f32, y: f32, width: f32, height: f32, rgba: u32) -> Path<ScaledPixels> {
    let mut path = Path::new(point(px(x), px(y)));
    path.line_to(point(px(x + width), px(y)));
    path.line_to(point(px(x + width), px(y + height)));
    path.line_to(point(px(x), px(y + height)));
    path.line_to(point(px(x), px(y)));
    path.color = color(rgba).into();
    path.content_mask = ContentMask {
        bounds: Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(4096.), px(4096.)),
        },
    };
    path.scale(1.)
}

fn rectangle_quad(x: f32, y: f32, width: f32, height: f32, rgba: u32) -> Quad {
    Quad {
        bounds: bounds(x, y, width, height),
        content_mask: everywhere(),
        background: color(rgba).into(),
        ..Default::default()
    }
}

/// Two rectangles at `(x, y)`, with a quad drawn between them so that each is
/// rasterized in a path pass of its own, drawn as paths or as quads.
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
    scene.insert_primitive(rectangle_quad(x + 4., y + 20., 8., 8., 0x00ff00ff));
    rectangle(&mut scene, x + 12., y + 8., BLUE);
    scene.finish();
    scene
}

fn assert_same_pixels(paths: &image::RgbaImage, quads: &image::RgbaImage, frame: usize) {
    assert_eq!(paths.dimensions(), quads.dimensions());
    for (x, y, path) in paths.enumerate_pixels() {
        let quad = quads.get_pixel(x, y);
        assert!(
            path.0.iter().zip(quad.0).all(|(a, b)| a.abs_diff(b) <= 3),
            "frame {frame}, pixel ({x}, {y}): drawn as a path {:?}, as a quad {:?}",
            path.0,
            quad.0
        );
    }
}

/// Rectangles on whole pixels cover every sample of a pixel or none, so drawn
/// as paths through the multisampled pass they must come out exactly as quads
/// do: what the resolve writes is all the frame needs of the samples. Each
/// path pass starts from a cleared attachment, so the second frame, drawn
/// elsewhere, shows nothing of the first.
#[test]
fn multisampled_paths_on_whole_pixels_draw_as_quads_do() {
    let Ok(mut renderer) = WgpuHeadlessRenderer::new() else {
        eprintln!("skipped: no wgpu adapter");
        return;
    };
    assert!(
        renderer.core.rendering_params.path_sample_count > 1,
        "the adapter rasterizes paths without multisampling, so the test shows nothing"
    );
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

/// GPU time of frames full of paths at window sizes, in 8 path passes split
/// by quads drawn between them, waiting for each frame to complete, without
/// readback.
#[test]
#[ignore = "manual GPU performance measurement"]
fn path_pass_frame_cost() {
    let mut renderer = WgpuHeadlessRenderer::new().expect("timing requires a GPU");
    eprintln!(
        "path sample count {}",
        renderer.core.rendering_params.path_sample_count
    );
    for (width, height) in [(1440u32, 900u32), (2880, 1800)] {
        let mut scene = Scene::default();
        let rows = height / 64;
        for row in 0..rows {
            if row > 0 && row % rows.div_ceil(8) == 0 {
                scene.insert_primitive(rectangle_quad(0., row as f32 * 64., 4., 4., BLUE));
            }
            for column in 0..width / 64 {
                let rgba = if (row + column) % 2 == 0 { RED } else { BLUE };
                scene.insert_primitive(rectangle_path(
                    column as f32 * 64. + 8.,
                    row as f32 * 64. + 8.,
                    48.,
                    48.,
                    rgba,
                ));
            }
        }
        scene.finish();
        let target = size(DevicePixels(width as i32), DevicePixels(height as i32));
        let mut samples = Vec::new();
        for frame in 0..120 {
            let start = std::time::Instant::now();
            renderer.render_scene(&scene, target).unwrap();
            renderer
                .core
                .resources
                .device
                .poll(wgpu::PollType::Wait {
                    submission_index: None,
                    timeout: None,
                })
                .unwrap();
            if frame >= 20 {
                samples.push(start.elapsed());
            }
        }
        samples.sort();
        eprintln!(
            "{width}x{height}, {} paths: median {:?}, p90 {:?}",
            (width / 64) * (height / 64),
            samples[samples.len() / 2],
            samples[samples.len() * 9 / 10]
        );
    }
}
