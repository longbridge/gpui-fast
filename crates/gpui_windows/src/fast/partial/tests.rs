//! Partial redraws on Direct3D 11: a scene drawn into the damage of a canvas
//! holding the scene before it must come out byte for byte as the scene
//! drawn whole, and the plan must draw whole whenever the canvas may not
//! hold the scene the damage refers to.
//!
//! [`Rig`] draws without a window: the renderer's own devices and resources
//! on a composition swap chain (which needs no window), its pipelines and
//! atlas, and a canvas it reads back. Where no device can be created the
//! pixel tests skip.

use std::borrow::Cow;
use std::slice;

use anyhow::{Context as _, Result};
use gpui::{
    AtlasKey, AtlasTile, Bounds, ContentMask, DevicePixels, FontId, GlyphId, Hsla,
    MonochromeSprite, Path, PlatformAtlas, Quad, RenderGlyphParams, ScaledPixels, Scene, Shadow,
    TransformationMatrix, Underline, point, px, rgba, size,
};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Buffer, ID3D11RasterizerState, ID3D11RenderTargetView, ID3D11Texture2D,
};

use super::{Frame, Plan, Whole, create_canvas_texture, create_scissor_state, draw_rects, plan};
use crate::DirectXAtlas;
use crate::directx_devices::DirectXDevices;
use crate::directx_renderer::{
    DirectXGlobalElements, DirectXRenderPipelines, DirectXRendererDevices, DirectXResources,
};
use crate::fast::frame::{FrameState, Target, draw_scene, upload};
use crate::fast::layers::tests::{assert_same_pixels, device_size, no_mask, quad, read_back, sp};
use crate::fast::layers::tile_cache::create_globals;

const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;

// --- the plan --- //

fn frame<'a>(damage: &'a [Bounds<DevicePixels>]) -> Frame<'a> {
    Frame {
        has_canvas: true,
        width: WIDTH,
        height: HEIGHT,
        number: 8,
        since: 7,
        last_drawn: 7,
        appearance_changed: false,
        atlas_written: false,
        has_surfaces: false,
        capturing: false,
        damage,
    }
}

fn rect(x: i32, y: i32, w: i32, h: i32) -> Bounds<DevicePixels> {
    Bounds {
        origin: point(DevicePixels(x), DevicePixels(y)),
        size: size(DevicePixels(w), DevicePixels(h)),
    }
}

#[test]
fn damage_relative_to_the_canvas_draws_partially() {
    let damage = [rect(10, 10, 20, 20), rect(300, 230, 40, 40)];
    assert_eq!(
        plan(&frame(&damage)),
        Plan::Partial(vec![
            RECT {
                left: 10,
                top: 10,
                right: 30,
                bottom: 30
            },
            // Clamped to the window.
            RECT {
                left: 300,
                top: 230,
                right: 320,
                bottom: 240
            },
        ])
    );
    assert_eq!(plan(&frame(&[])), Plan::Partial(Vec::new()));
}

/// Overlapping rectangles become their union, and the union absorbs what it
/// then overlaps, so no pixel is drawn twice.
#[test]
fn overlapping_damage_is_merged() {
    let damage = [
        rect(10, 10, 20, 20),
        rect(25, 25, 20, 20),
        rect(44, 5, 10, 10),
        rect(100, 100, 5, 5),
    ];
    let Plan::Partial(rects) = plan(&frame(&damage)) else {
        panic!("drawn whole");
    };
    let mut rects: Vec<(i32, i32, i32, i32)> = rects
        .iter()
        .map(|r| (r.left, r.top, r.right, r.bottom))
        .collect();
    rects.sort();
    assert_eq!(rects, [(10, 5, 54, 45), (100, 100, 105, 105)]);
}

#[test]
fn frames_the_canvas_may_not_hold_draw_whole() {
    let damage = [rect(10, 10, 20, 20)];
    let cases = [
        (
            Frame {
                has_canvas: false,
                ..frame(&damage)
            },
            Whole::NoCanvas,
        ),
        (
            Frame {
                number: 0,
                ..frame(&damage)
            },
            Whole::NotNumbered,
        ),
        (
            Frame {
                since: 0,
                ..frame(&damage)
            },
            Whole::NotComparable,
        ),
        (
            Frame {
                last_drawn: 6,
                ..frame(&damage)
            },
            Whole::NotComparable,
        ),
        (
            Frame {
                appearance_changed: true,
                ..frame(&damage)
            },
            Whole::Appearance,
        ),
        (
            Frame {
                atlas_written: true,
                ..frame(&damage)
            },
            Whole::AtlasWritten,
        ),
        (
            Frame {
                has_surfaces: true,
                ..frame(&damage)
            },
            Whole::Surfaces,
        ),
        (
            Frame {
                capturing: true,
                ..frame(&damage)
            },
            Whole::Capturing,
        ),
    ];
    for (frame, reason) in cases {
        assert_eq!(plan(&frame), Plan::Whole(reason), "{frame:?}");
    }
    let large = [rect(0, 0, 320, 121)];
    assert_eq!(plan(&frame(&large)), Plan::Whole(Whole::LargeDamage));
}

// --- pixels --- //

/// A change of each kind, over a backdrop of every kind, with its damage
/// generous around it, as `fast::damage` reports it: `(before, after,
/// damage)`.
#[test]
fn a_partial_frame_equals_the_scene_drawn_whole() {
    let Some(mut rig) = Rig::new() else {
        eprintln!("skipped: no Direct3D 11 device");
        return;
    };
    let glyph = rig.glyph(1);
    let other_glyph = rig.glyph(2);
    let before = workspace(Change::Before, glyph, other_glyph);
    let after = workspace(Change::After, glyph, other_glyph);
    let damage = [
        // The quad changing color, an edge of the translucent panel over it.
        RECT {
            left: 20,
            top: 20,
            right: 70,
            bottom: 60,
        },
        // The glyph moving, over a path's edge.
        RECT {
            left: 150,
            top: 100,
            right: 200,
            bottom: 130,
        },
        // The card and its shadow moving.
        RECT {
            left: 200,
            top: 150,
            right: 310,
            bottom: 235,
        },
    ];
    for clear in [[1.0f32; 4], [0.0; 4]] {
        let mut canvas = rig.canvas().expect("canvas");
        rig.draw_whole(&mut canvas, &before, clear).expect("drawn");
        rig.draw_partial(&mut canvas, &after, clear, &damage)
            .expect("drawn");
        let partial = read_back(
            &rig.devices.device,
            &rig.devices.device_context,
            &canvas.texture,
        )
        .expect("read back");

        let mut fresh = rig.canvas().expect("canvas");
        rig.draw_whole(&mut fresh, &after, clear).expect("drawn");
        let whole = read_back(
            &rig.devices.device,
            &rig.devices.device_context,
            &fresh.texture,
        )
        .expect("read back");
        assert_same_pixels(&partial, &whole, WIDTH as usize);
    }
}

/// Several rectangles over several path batches, each path crossing more
/// than one rectangle: each batch is rasterized once and drawn through every
/// rectangle, and the frame must still come out as the scene drawn whole.
#[test]
fn paths_across_several_rectangles_equal_the_scene_drawn_whole() {
    let Some(mut rig) = Rig::new() else {
        eprintln!("skipped: no Direct3D 11 device");
        return;
    };
    let before = paths_scene(Change::Before);
    let after = paths_scene(Change::After);
    let damage = [
        RECT {
            left: 30,
            top: 40,
            right: 90,
            bottom: 100,
        },
        RECT {
            left: 120,
            top: 60,
            right: 170,
            bottom: 140,
        },
        RECT {
            left: 200,
            top: 30,
            right: 260,
            bottom: 90,
        },
    ];
    for clear in [[1.0f32; 4], [0.0; 4]] {
        let mut canvas = rig.canvas().expect("canvas");
        rig.draw_whole(&mut canvas, &before, clear).expect("drawn");
        rig.draw_partial(&mut canvas, &after, clear, &damage)
            .expect("drawn");
        let partial = read_back(
            &rig.devices.device,
            &rig.devices.device_context,
            &canvas.texture,
        )
        .expect("read back");

        let mut fresh = rig.canvas().expect("canvas");
        rig.draw_whole(&mut fresh, &after, clear).expect("drawn");
        let whole = read_back(
            &rig.devices.device,
            &rig.devices.device_context,
            &fresh.texture,
        )
        .expect("read back");
        assert_same_pixels(&partial, &whole, WIDTH as usize);
    }
}

/// Translucent paths in three batches (a quad between each two), each
/// crossing several of the damage rectangles of
/// `paths_across_several_rectangles_equal_the_scene_drawn_whole`, and a quad
/// inside each rectangle that changes color.
fn paths_scene(change: Change) -> Scene {
    let after = change == Change::After;
    let mut scene = Scene::default();
    scene.insert_primitive(quad(sp(0., 0., 320., 240.), Hsla::from(rgba(0x20242aff))));
    let band = |top: f32, bottom: f32, color: u32| {
        let mut path = Path::new(point(px(10.), px(top)));
        path.line_to(point(px(300.), px(top + 20.)));
        path.curve_to(
            point(px(310.), px(bottom)),
            point(px(200.), px(bottom + 30.)),
        );
        path.line_to(point(px(10.), px(bottom)));
        path.line_to(point(px(10.), px(top)));
        path.color = rgba(color).into();
        path.content_mask = ContentMask {
            bounds: Bounds {
                origin: point(px(-1000.), px(-1000.)),
                size: size(px(3000.), px(3000.)),
            },
        };
        path.scale(1.)
    };
    scene.insert_primitive(band(50., 90., 0x44cc88c0));
    scene.insert_primitive(quad(sp(20., 45., 280., 30.), Hsla::from(rgba(0xffffff40))));
    scene.insert_primitive(band(60., 120., 0xcc4488a0));
    scene.insert_primitive(quad(sp(20., 70., 280., 30.), Hsla::from(rgba(0x3080ff50))));
    scene.insert_primitive(band(40., 110., 0xeecc2280));
    for (x, y) in [(50., 60.), (135., 100.), (220., 50.)] {
        let color = if after { 0x3366ffff } else { 0xff3344ff };
        scene.insert_primitive(quad(sp(x, y, 16., 16.), Hsla::from(rgba(color))));
    }
    scene.finish();
    scene
}

/// Without damage the canvas keeps the frame before, pixel for pixel.
#[test]
fn a_frame_without_damage_keeps_the_canvas() {
    let Some(mut rig) = Rig::new() else {
        eprintln!("skipped: no Direct3D 11 device");
        return;
    };
    let glyph = rig.glyph(1);
    let other_glyph = rig.glyph(2);
    let scene = workspace(Change::Before, glyph, other_glyph);
    let mut canvas = rig.canvas().expect("canvas");
    rig.draw_whole(&mut canvas, &scene, [1.0; 4])
        .expect("drawn");
    let before = read_back(
        &rig.devices.device,
        &rig.devices.device_context,
        &canvas.texture,
    )
    .expect("read back");
    rig.draw_partial(&mut canvas, &scene, [1.0; 4], &[])
        .expect("drawn");
    let after = read_back(
        &rig.devices.device,
        &rig.devices.device_context,
        &canvas.texture,
    )
    .expect("read back");
    assert_same_pixels(&after, &before, WIDTH as usize);
}

#[derive(Clone, Copy, PartialEq)]
enum Change {
    Before,
    After,
}

/// A window of every primitive kind: a backdrop, a quad that changes
/// color, a translucent panel over it, paths, an underline, glyphs, one
/// moving, and a card whose shadow moves with it.
fn workspace(change: Change, glyph: AtlasTile, other_glyph: AtlasTile) -> Scene {
    let after = change == Change::After;
    let mut scene = Scene::default();
    scene.insert_primitive(quad(sp(0., 0., 320., 240.), Hsla::from(rgba(0x20242aff))));
    let color = if after { 0x3366ffff } else { 0xff3344ff };
    scene.insert_primitive(quad(sp(24., 24., 30., 20.), Hsla::from(rgba(color))));
    scene.insert_primitive(Quad {
        corner_radii: gpui::Corners::all(ScaledPixels(6.)),
        ..quad(sp(40., 10., 120., 60.), Hsla::from(rgba(0xffffff40)))
    });
    // A path crossing the second damage rectangle's edge.
    let mut path = Path::new(point(px(120.), px(90.)));
    path.line_to(point(px(190.), px(95.)));
    path.curve_to(point(px(140.), px(140.)), point(px(200.), px(130.)));
    path.line_to(point(px(120.), px(90.)));
    path.color = rgba(0x44cc88c0).into();
    path.content_mask = ContentMask {
        bounds: Bounds {
            origin: point(px(-1000.), px(-1000.)),
            size: size(px(3000.), px(3000.)),
        },
    };
    scene.insert_primitive(path.scale(1.));
    scene.insert_primitive(Underline {
        order: 0,
        pad: 0,
        bounds: sp(10., 200., 150., 2.),
        content_mask: no_mask(),
        color: Hsla::from(rgba(0xffcc00ff)),
        thickness: ScaledPixels(2.),
        wavy: false.into(),
    });
    for (x, tile) in [
        (100., other_glyph),
        (if after { 175. } else { 160. }, glyph),
    ] {
        scene.insert_primitive(MonochromeSprite {
            order: 0,
            pad: 0,
            bounds: sp(x, 105., 16., 16.),
            content_mask: no_mask(),
            color: Hsla::from(rgba(0xf0f0f0ff)),
            tile,
            transformation: TransformationMatrix::unit(),
        });
    }
    let card_x = if after { 240. } else { 220. };
    scene.insert_primitive(Shadow {
        order: 0,
        blur_radius: ScaledPixels(6.),
        bounds: sp(card_x, 170., 50., 40.),
        corner_radii: gpui::Corners::all(ScaledPixels(4.)),
        content_mask: no_mask(),
        color: Hsla::from(rgba(0x00000099)),
        element_bounds: sp(card_x, 170., 50., 40.),
        element_corner_radii: gpui::Corners::all(ScaledPixels(4.)),
        inset: 0,
        pad: 0,
    });
    scene.insert_primitive(quad(
        sp(card_x, 170., 50., 40.),
        Hsla::from(rgba(0xe0e0e0ff)),
    ));
    scene.finish();
    scene
}

/// The renderer's objects, without a window.
struct Rig {
    devices: DirectXRendererDevices,
    resources: DirectXResources,
    globals: DirectXGlobalElements,
    pipelines: DirectXRenderPipelines,
    atlas: DirectXAtlas,
    frame: FrameState,
    viewport_globals: ID3D11Buffer,
    scissor_state: ID3D11RasterizerState,
}

struct Canvas {
    texture: ID3D11Texture2D,
    view: Option<ID3D11RenderTargetView>,
}

impl Rig {
    fn new() -> Option<Rig> {
        let directx_devices = DirectXDevices::new()
            .inspect_err(|error| eprintln!("no devices: {error:#}"))
            .ok()?;
        let devices = DirectXRendererDevices::new(&directx_devices, false).ok()?;
        // A composition swap chain, which needs no window; `new` also sets
        // the renderer's rasterizer state.
        let resources = DirectXResources::new(&devices, WIDTH, HEIGHT, HWND::default(), false)
            .inspect_err(|error| eprintln!("no resources: {error:#}"))
            .ok()?;
        let pipelines = DirectXRenderPipelines::new(&devices.device).ok()?;
        let globals = DirectXGlobalElements::new(&devices.device).ok()?;
        let atlas = DirectXAtlas::new(&devices.device, &devices.device_context);
        let viewport_globals = create_globals(&devices.device, WIDTH, HEIGHT).ok()??;
        let scissor_state = create_scissor_state(&devices.device, &devices.device_context).ok()?;
        Some(Rig {
            devices,
            resources,
            globals,
            pipelines,
            atlas,
            frame: FrameState::default(),
            viewport_globals,
            scissor_state,
        })
    }

    /// A 16×16 glyph of a made-up font, uploaded to the atlas.
    fn glyph(&self, glyph: u32) -> AtlasTile {
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
        let bytes: Vec<u8> = (0..16 * 16u32)
            .map(|i| ((i * (37 + glyph)) % 256) as u8)
            .collect();
        self.atlas
            .get_or_insert_with(key, &mut || {
                Ok(Some((device_size(16, 16), Cow::Owned(bytes.clone()))))
            })
            .expect("glyph uploaded")
            .expect("glyph tile")
    }

    fn canvas(&self) -> Result<Canvas> {
        let texture = create_canvas_texture(&self.devices.device, WIDTH, HEIGHT)?;
        let mut view = None;
        unsafe {
            self.devices
                .device
                .CreateRenderTargetView(&texture, None, Some(&mut view))?
        };
        Ok(Canvas { texture, view })
    }

    /// Lends the canvas's view to the resources, as the renderer does, binds
    /// it as `DirectXRenderer::pre_draw` binds the window's, runs `draw` and
    /// takes the view back.
    fn with_canvas(
        &mut self,
        canvas: &mut Canvas,
        clear: Option<[f32; 4]>,
        draw: impl FnOnce(&mut Rig, &ID3D11RenderTargetView) -> Result<()>,
    ) -> Result<()> {
        std::mem::swap(&mut self.resources.render_target_view, &mut canvas.view);
        let drawn = (|| {
            let view = self
                .resources
                .render_target_view
                .clone()
                .context("canvas view")?;
            let context = &self.devices.device_context;
            unsafe {
                if let Some(clear) = clear {
                    context.ClearRenderTargetView(&view, &clear);
                }
                context.OMSetRenderTargets(
                    Some(slice::from_ref(&self.resources.render_target_view)),
                    None,
                );
                context.RSSetViewports(Some(slice::from_ref(&self.resources.viewport)));
                context.VSSetConstantBuffers(0, Some(&[Some(self.viewport_globals.clone())]));
                context.VSSetConstantBuffers(
                    1,
                    Some(slice::from_ref(&self.globals.batch_params_buffer)),
                );
                context.PSSetConstantBuffers(0, Some(&[Some(self.viewport_globals.clone())]));
            }
            draw(self, &view)
        })();
        unsafe { self.devices.device_context.OMSetRenderTargets(None, None) };
        std::mem::swap(&mut self.resources.render_target_view, &mut canvas.view);
        drawn
    }

    fn draw_whole(&mut self, canvas: &mut Canvas, scene: &Scene, clear: [f32; 4]) -> Result<()> {
        self.with_canvas(canvas, Some(clear), |rig, _| {
            let target = Target {
                device: &rig.devices.device,
                device_context: &rig.devices.device_context,
                atlas: &rig.atlas,
                globals: &rig.globals,
                resources: Some(&rig.resources),
            };
            upload(&target, &mut rig.pipelines, scene)?;
            draw_scene(&target, &mut rig.pipelines, &mut rig.frame, scene)
        })
    }

    fn draw_partial(
        &mut self,
        canvas: &mut Canvas,
        scene: &Scene,
        clear: [f32; 4],
        rects: &[RECT],
    ) -> Result<()> {
        self.with_canvas(canvas, None, |rig, view| {
            let target = Target {
                device: &rig.devices.device,
                device_context: &rig.devices.device_context,
                atlas: &rig.atlas,
                globals: &rig.globals,
                resources: Some(&rig.resources),
            };
            draw_rects(
                &target,
                &mut rig.pipelines,
                &mut rig.frame,
                scene,
                view,
                &rig.scissor_state,
                clear,
                rects,
            )
        })
    }
}

// --- the back buffers --- //

fn r(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
    RECT {
        left,
        top,
        right,
        bottom,
    }
}

/// A back buffer copies the changes of the frames presented since it was
/// last presented, and the whole canvas until every buffer was presented
/// once, after another present of the swap chain, or a failed one.
#[test]
fn back_buffers_copy_what_changed_since_they_were_presented() {
    use super::back_buffers::BackBuffers;
    /// A swap chain's present count and what our frames presented.
    struct Chain {
        buffers: BackBuffers,
        count: u32,
    }
    impl Chain {
        fn present(&mut self, changed: Option<RECT>, succeeded: bool) -> Option<Vec<RECT>> {
            let changed = changed.map(|rect| vec![rect]);
            let copy = BackBuffers::to_copy(
                &mut self.buffers,
                changed.as_deref(),
                self.count,
                WIDTH,
                HEIGHT,
            );
            BackBuffers::presented(
                &mut self.buffers,
                changed,
                succeeded,
                self.count,
                self.count + 1,
            );
            self.count += 1;
            copy.map(|mut rects| {
                rects.sort_by_key(|rect| rect.left);
                rects
            })
        }

        fn frame(&mut self, rect: RECT) -> Option<Vec<RECT>> {
            self.present(Some(rect), true)
        }
    }
    let mut chain = Chain {
        buffers: BackBuffers::default(),
        count: 10,
    };
    let rects = [
        r(0, 0, 10, 10),
        r(20, 0, 30, 10),
        r(40, 0, 50, 10),
        r(60, 0, 70, 10),
        r(80, 0, 90, 10),
    ];
    // Every buffer is presented once before any is trusted.
    for rect in &rects[..3] {
        assert_eq!(chain.frame(*rect), None);
    }
    // The fourth frame's buffer holds the first: the two frames between
    // and its own change.
    assert_eq!(chain.frame(rects[3]), Some(vec![rects[1], rects[2], rects[3]]));
    assert_eq!(chain.frame(rects[4]), Some(vec![rects[2], rects[3], rects[4]]));

    // A whole frame, presented: the next two copy everything.
    assert_eq!(chain.present(None, true), None);
    assert_eq!(chain.frame(rects[0]), None);
    assert_eq!(chain.frame(rects[1]), None);
    assert!(chain.frame(rects[2]).is_some());

    // Someone else presented: start over.
    chain.count += 1;
    for rect in &rects[..3] {
        assert_eq!(chain.frame(*rect), None);
    }
    assert!(chain.frame(rects[3]).is_some());

    // A present that did not return S_OK: start over.
    chain.present(Some(rects[0]), false);
    assert_eq!(chain.frame(rects[1]), None);

    // Changes covering more than half the window: copy everything.
    let mut chain = Chain {
        buffers: BackBuffers::default(),
        count: 0,
    };
    for _ in 0..3 {
        chain.frame(rects[0]);
    }
    assert_eq!(chain.frame(r(0, 0, WIDTH as i32, HEIGHT as i32 / 2 + 10)), None);
}

/// Copied as `BackBuffers` says, frame after frame on a real flip-sequential
/// swap chain, the back buffer always comes out as the canvas: DXGI hands
/// the buffers out in turn, keeping what they held.
#[test]
fn back_buffers_copied_partially_hold_the_canvas() {
    use super::back_buffers::BackBuffers;
    let Some(mut rig) = Rig::new() else {
        eprintln!("skipped: no Direct3D 11 device");
        return;
    };
    let slot = |i: usize| r(10 + 30 * i as i32, 20, 30 + 30 * i as i32, 40);
    let scene = |frame: usize| {
        let mut scene = Scene::default();
        scene.insert_primitive(quad(sp(0., 0., 320., 240.), Hsla::from(rgba(0x20242aff))));
        for i in 0..9 {
            let color = if i < frame { 0x3366ffff } else { 0xff3344ff };
            let rect = slot(i);
            scene.insert_primitive(quad(
                sp(rect.left as f32, rect.top as f32, 20., 20.),
                Hsla::from(rgba(color)),
            ));
        }
        scene.finish();
        scene
    };
    let clear = [1.0f32; 4];
    let mut canvas = rig.canvas().expect("canvas");
    let mut buffers = BackBuffers::default();
    let swap_chain = rig.resources.swap_chain.clone();
    let back_buffer = rig.resources.render_target.clone().expect("back buffer");
    let mut partial_copies = 0;
    for frame in 0..9 {
        let changed = if frame == 0 {
            rig.draw_whole(&mut canvas, &scene(0), clear).expect("drawn");
            None
        } else {
            let rects = vec![slot(frame - 1)];
            rig.draw_partial(&mut canvas, &scene(frame), clear, &rects)
                .expect("drawn");
            Some(rects)
        };
        let count = unsafe { swap_chain.GetLastPresentCount() }.expect("present count");
        let copy = BackBuffers::to_copy(&mut buffers, changed.as_deref(), count, WIDTH, HEIGHT);
        partial_copies += usize::from(copy.is_some());
        super::copy_rects(
            &rig.devices.device_context,
            &back_buffer,
            &canvas.texture,
            copy.as_deref(),
        );
        let read = |texture| {
            read_back(&rig.devices.device, &rig.devices.device_context, texture)
                .expect("read back")
        };
        assert_same_pixels(&read(&back_buffer), &read(&canvas.texture), WIDTH as usize);
        let result = match &changed {
            Some(rects) => super::present_dirty(&swap_chain, rects),
            None => unsafe {
                swap_chain.Present(0, windows::Win32::Graphics::Dxgi::DXGI_PRESENT(0))
            },
        };
        let after = unsafe { swap_chain.GetLastPresentCount() }.expect("present count");
        BackBuffers::presented(
            &mut buffers,
            changed,
            result == windows::Win32::Foundation::S_OK,
            count,
            after,
        );
    }
    assert!(partial_copies >= 4, "{partial_copies} partial copies");
}

// --- atlas writes --- //

/// Only the sprites whose tile overlaps a write join the damage, and a write
/// inside the damage adds nothing.
#[test]
fn atlas_writes_damage_the_sprites_that_show_them() {
    use gpui::{AtlasTextureId, AtlasTextureKind, TileId};

    use super::atlas_writes::{count, note, since, written_sprites};
    use super::with_written;

    let tile = |index: u32, x: i32| AtlasTile {
        texture_id: AtlasTextureId {
            index,
            kind: AtlasTextureKind::Monochrome,
        },
        tile_id: TileId(x as u32),
        padding: 0,
        bounds: rect(x, 0, 16, 16),
    };
    let sprite = |tile: AtlasTile, x: f32| MonochromeSprite {
        order: 0,
        pad: 0,
        bounds: sp(x, 10., 16., 16.),
        content_mask: no_mask(),
        color: Hsla::from(rgba(0x000000ff)),
        tile,
        transformation: TransformationMatrix::unit(),
    };
    let mut scene = Scene::default();
    scene.insert_primitive(sprite(tile(70, 0), 10.));
    scene.insert_primitive(sprite(tile(70, 32), 100.));
    scene.insert_primitive(sprite(tile(71, 0), 200.));
    scene.finish();

    let seen = count();
    note(&tile(70, 32));
    // Other tests write to atlases too: keep only this test's texture.
    let writes: Vec<_> = since(seen)
        .expect("logged")
        .into_iter()
        .filter(|(id, _)| id.index == 70)
        .collect();
    assert_eq!(written_sprites(&scene, &writes), [rect(99, 9, 18, 18)]);
    // More writes than the log keeps: the frame cannot know them all.
    let before = count();
    for _ in 0..300 {
        note(&tile(72, 0));
    }
    assert!(since(before).is_none());

    let damage = [rect(0, 0, 50, 50)];
    assert_eq!(
        with_written(&damage, vec![rect(5, 5, 10, 10), rect(99, 9, 18, 18)]),
        [rect(0, 0, 50, 50), rect(99, 9, 18, 18)]
    );
    let many: Vec<_> = (0..20).map(|i| rect(100 + 20 * i, 100, 10, 10)).collect();
    assert_eq!(
        with_written(&damage, many),
        [rect(0, 0, 50, 50), rect(100, 100, 390, 10)]
    );
}
