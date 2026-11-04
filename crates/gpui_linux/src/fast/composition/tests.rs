use super::whole_scene_layer;
use gpui::PlatformHeadlessRenderer as _;
use gpui::{
    Bounds, ComposedSceneLayer, CompositionSurfaceId, ContentMask, Quad, ScaledPixels, Scene,
    point, size,
};
use gpui_wgpu::WgpuHeadlessRenderer;

#[gpui::test]
fn composition_whole_scene_matches_combined_window_pixels(cx: &mut gpui::TestAppContext) {
    struct View;
    impl gpui::Render for View {
        fn render(
            &mut self,
            _: &mut gpui::Window,
            _: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            use gpui::{ParentElement as _, Styled as _};
            gpui::div().size(gpui::px(64.)).bg(gpui::white()).child(
                gpui::div()
                    .size(gpui::px(64.))
                    .rounded(gpui::px(24.))
                    .overflow_hidden()
                    .child(gpui::div().size(gpui::px(64.)).bg(gpui::black())),
            )
        }
    }
    use gpui::AppContext as _;
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| cx.new(|_| View))
            .unwrap()
    });
    let mut renderer = WgpuHeadlessRenderer::new().expect("composition regression requires WGPU");
    window
        .update(cx, |_, window, cx| {
            window.draw(cx).clear(cx);
            let scene = window.painted_scene();
            assert!(
                !scene.layers.frames.is_empty(),
                "rounded clips were compiled"
            );
            let full = gpui::ComposedSceneLayer {
                surface: gpui::CompositionSurfaceId::default(),
                ranges: vec![0..scene.len()],
            };
            let empty = gpui::ComposedSceneLayer {
                ranges: vec![],
                ..full.clone()
            };
            for (layers, expected_target) in [
                (vec![full.clone(), empty.clone()], 0),
                (vec![empty.clone(), full.clone()], 1),
                (vec![full.clone()], 0),
                (vec![empty.clone(), full, empty], 1),
            ] {
                let (target_index, borrowed) = whole_scene_layer(scene, &layers).unwrap();
                assert_eq!(target_index, expected_target);
                assert!(std::ptr::eq(borrowed, scene));
                // X11's old window_scene includes ALL layers in order,
                // including paint also sent to its overlay window. With one
                // full target this is also the old Wayland target output.
                let mut replayed = Scene::default();
                for layer in &layers {
                    for range in &layer.ranges {
                        replayed.replay_balanced(range.clone(), scene).unwrap();
                    }
                }
                replayed.finish();
                let side = (64. * window.scale_factor()) as i32;
                let extent = size(gpui::DevicePixels(side), gpui::DevicePixels(side));
                let expected = renderer.render_scene_to_image(&replayed, extent).unwrap();
                let actual = renderer.render_scene_to_image(borrowed, extent).unwrap();
                assert_eq!(actual.as_raw(), expected.as_raw());
                assert_eq!(actual.get_pixel(1, 1).0, [255, 255, 255, 255]);
                assert_eq!(
                    actual.get_pixel(side as u32 / 2, side as u32 / 2).0,
                    [0, 0, 0, 255]
                );
            }
        })
        .unwrap();
}

#[test]
fn partial_or_split_layers_require_replay_even_when_primitives_match() {
    let mut scene = Scene::default();
    let bounds = Bounds::new(
        point(ScaledPixels(0.), ScaledPixels(0.)),
        size(ScaledPixels(64.), ScaledPixels(64.)),
    );
    scene.push_layer(bounds);
    scene.insert_primitive(Quad {
        bounds,
        content_mask: ContentMask { bounds },
        ..Default::default()
    });
    scene.pop_layer();
    scene.finish();
    let len = scene.len();
    let layer = |ranges| ComposedSceneLayer {
        surface: CompositionSurfaceId::default(),
        ranges,
    };
    // The inner range has the same sole primitive, but must restore its layer.
    for layers in [
        vec![layer(vec![1..len - 1])],
        vec![layer(vec![0..len - 1])],
        vec![layer(vec![1..len])],
        vec![layer(vec![0..1, 1..len])],
        vec![layer(vec![0..len]), layer(vec![0..len])],
        vec![layer(vec![0..len]), layer(vec![1..len - 1])],
        vec![layer(vec![0..len]), layer(vec![0..0])],
        vec![layer(vec![])],
        vec![layer(vec![]), layer(vec![])],
        vec![layer(vec![0..len + 1])],
        vec![],
    ] {
        assert!(whole_scene_layer(&scene, &layers).is_none(), "{layers:?}");
    }

    let mut empty = Scene::default();
    empty.finish();
    assert!(whole_scene_layer(&empty, &[layer(vec![])]).is_none());
    for (layers, expected_target) in [
        (vec![layer(vec![0..0])], 0),
        (vec![layer(vec![]), layer(vec![0..0]), layer(vec![])], 1),
    ] {
        let (target_index, borrowed) = whole_scene_layer(&empty, &layers).unwrap();
        assert_eq!(target_index, expected_target);
        assert!(std::ptr::eq(borrowed, &empty));
    }
    assert!(whole_scene_layer(&empty, &[layer(vec![0..0]), layer(vec![0..0])]).is_none());
}
