//! Exercise the WebGL instance-texture transport on the native GPU as well:
//! masks, paths and nested scenes must use the same absolute record indices.

use super::{assert_same_pixels, pixel};

#[path = "../../../../gpui/src/fast/rounded_clip_fixtures.rs"]
mod fixtures;

fn texture_renderer() -> crate::wgpu_renderer::WgpuHeadlessRenderer {
    let mut renderer = crate::wgpu_renderer::WgpuHeadlessRenderer::new()
        .expect("instance-texture tests require a GPU");
    let core = &mut renderer.core;
    let device = &core.resources.device;
    let layouts = crate::wgpu_renderer::WgpuRendererCore::create_bind_group_layouts(device, true);
    let pipelines = crate::wgpu_renderer::WgpuRendererCore::create_pipelines(
        device,
        &layouts,
        core.target_format,
        wgpu::CompositeAlphaMode::Opaque,
        core.rendering_params.path_sample_count,
        false,
        true,
    );
    let (instances, capacity) = crate::wgpu_renderer::WgpuRendererCore::create_instance_texture(
        device,
        1024,
        device.limits().max_texture_dimension_2d,
    );
    core.resources.bind_group_layouts = layouts;
    core.resources.pipelines = pipelines;
    core.resources.instance_data = instances;
    core.instance_data_capacity = capacity;
    core.instance_data_alignment = 16;
    core.uses_webgl_instance_data = true;
    core.fast_frame = Default::default();
    renderer
}

fn render_div_fixture(cx: &mut gpui::TestAppContext, build: fn() -> gpui::Div) -> (Vec<u8>, usize) {
    let mut renderer = texture_renderer();
    fixtures::render_with(cx, build, |scene, size| {
        gpui::PlatformHeadlessRenderer::render_scene_to_image(&mut renderer, scene, size)
            .expect("texture-transport frame rendered")
            .into_raw()
    })
}

#[gpui::test]
fn rounded_overflow_inside_scroll_tiles_matches_direct_pixels(cx: &mut gpui::TestAppContext) {
    let mut renderer = texture_renderer();
    fixtures::render_with(cx, fixtures::square_fixture, |scene, size| {
        let direct =
            gpui::PlatformHeadlessRenderer::render_scene_to_image(&mut renderer, scene, size)
                .unwrap()
                .into_raw();
        let tiled = fixtures::tiled_scene(scene, size);
        let actual =
            gpui::PlatformHeadlessRenderer::render_scene_to_image(&mut renderer, &tiled, size)
                .unwrap()
                .into_raw();
        assert_same_pixels(&actual, &direct, size.width.0 as usize);
        actual
    });
}
