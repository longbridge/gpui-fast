use std::sync::{Arc, Weak};

use gpui::{
    AppContext as _, Context, DevicePixels, IntoElement, ParentElement as _,
    PlatformHeadlessRenderer as _, Render, Styled as _, Window, canvas, div, px, size,
};

use crate::wgpu_renderer::WgpuRendererCore;
use crate::{WgpuAtlas, WgpuContext, WgpuDynamicTexture, WgpuHeadlessRenderer};

struct Showcase(Weak<WgpuDynamicTexture>);

impl Render for Showcase {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let texture = self.0.clone();
        div()
            .relative()
            .size(px(64.))
            .bg(gpui::white())
            .child(
                div().w(px(32.)).h(px(64.)).overflow_hidden().child(
                    canvas(
                        |_, _, _| (),
                        move |bounds, (), window, _| {
                            if let Some(texture) = texture.upgrade() {
                                texture.paint(window, bounds).expect("GPU texture paint");
                            }
                        },
                    )
                    .size(px(64.)),
                ),
            )
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size(px(16.))
                    .bg(gpui::black()),
            )
    }
}

fn clear(context: &WgpuContext, view: &wgpu::TextureView, color: wgpu::Color) {
    let mut encoder = context.device.create_command_encoder(&Default::default());
    encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            resolve_target: None,
            depth_slice: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(color),
                store: wgpu::StoreOp::Store,
            },
        })],
        ..Default::default()
    });
    context.queue.submit([encoder.finish()]);
}

fn pixel(image: &image::RgbaImage, scale: f32, x: f32, y: f32, expected: [u8; 4]) {
    let actual = image.get_pixel((x * scale) as u32, (y * scale) as u32).0;
    assert!(
        actual.iter().zip(expected).all(|(a, b)| a.abs_diff(b) <= 3),
        "pixel ({x},{y}): {actual:?} != {expected:?}"
    );
}

/// A second real device renders through the ordinary frame pipeline, with
/// readback only after rendering. Foreign sprite ids must not reach WGPU.
fn draw_other(
    context: &WgpuContext,
    format: wgpu::TextureFormat,
    scene: &gpui::Scene,
    side: u32,
) -> image::RgbaImage {
    let atlas = Arc::new(WgpuAtlas::from_context(context));
    let mut core = WgpuRendererCore::new(context, atlas, format, wgpu::CompositeAlphaMode::Opaque);
    let target = context.device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d {
            width: side,
            height: side,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    core.render_frame(
        scene,
        &target.create_view(&Default::default()),
        size(DevicePixels(side as i32), DevicePixels(side as i32)),
        false,
        wgpu::Color::BLACK,
    )
    .unwrap();
    let stride = (side * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let buffer = context.device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: u64::from(stride) * u64::from(side),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = context.device.create_command_encoder(&Default::default());
    encoder.copy_texture_to_buffer(
        target.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(stride),
                rows_per_image: Some(side),
            },
        },
        target.size(),
    );
    let submission = context.queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    buffer
        .slice(..)
        .map_async(wgpu::MapMode::Read, move |result| tx.send(result).unwrap());
    context
        .device
        .poll(wgpu::PollType::Wait {
            submission_index: Some(submission),
            timeout: Some(std::time::Duration::from_secs(10)),
        })
        .unwrap();
    rx.recv().unwrap().unwrap();
    let mapped = buffer
        .slice(..)
        .get_mapped_range()
        .expect("mapped readback");
    let mut bytes: Vec<u8> = mapped
        .chunks_exact(stride as usize)
        .flat_map(|row| row[..side as usize * 4].iter().copied())
        .collect();
    if format == wgpu::TextureFormat::Bgra8Unorm {
        for p in bytes.chunks_exact_mut(4) {
            p.swap(0, 2);
        }
    }
    drop(mapped);
    buffer.unmap();
    image::RgbaImage::from_raw(side, side, bytes).unwrap()
}

#[gpui::test]
fn gpu_written_textures_are_owned_clipped_and_context_local(cx: &mut gpui::TestAppContext) {
    let (context, _) = WgpuContext::new_headless().expect("GPU required");
    let (other, other_format) = WgpuContext::create_headless().expect("second GPU device");
    assert_ne!(context.device.as_ref(), other.device.as_ref());
    let mut renderer = WgpuHeadlessRenderer::new().unwrap();
    let mut owner = Some(Arc::new(
        context
            .create_dynamic_texture(size(DevicePixels(16), DevicePixels(16)))
            .unwrap(),
    ));
    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| {
            cx.new(|_| Showcase(Arc::downgrade(owner.as_ref().unwrap())))
        })
        .unwrap()
    });
    window
        .update(cx, |_, window, _| {
            owner.as_ref().unwrap().render(window, |view| {
                assert_eq!(
                    view.texture().usage(),
                    wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING
                );
                clear(&context, view, wgpu::Color::RED);
                Ok(())
            })
        })
        .unwrap()
        .unwrap();
    window
        .update(cx, |_, window, cx| {
            window.draw(cx).clear(cx);
            let scale = window.scale_factor();
            let side = DevicePixels((64. * scale) as i32);
            let image = renderer
                .render_scene_to_image(window.painted_scene(), size(side, side))
                .unwrap();
            pixel(&image, scale, 24., 40., [255, 0, 0, 255]);
            pixel(&image, scale, 8., 8., [0, 0, 0, 255]);
            pixel(&image, scale, 48., 40., [255, 255, 255, 255]);
            let mut epoch = other.errors().current_generation();
            let foreign = draw_other(&other, other_format, window.painted_scene(), side.0 as u32);
            pixel(&foreign, scale, 24., 40., [255, 255, 255, 255]);
            assert!(other.errors().observe_error(&mut epoch).is_none());
            drop(owner.take());
            // Reuse the exact painted scene, without redrawing or refreshing it.
            let dropped = renderer
                .render_scene_to_image(window.painted_scene(), size(side, side))
                .unwrap();
            pixel(&dropped, scale, 24., 40., [255, 255, 255, 255]);
        })
        .unwrap();

    let replacement = Arc::new(
        other
            .create_dynamic_texture(size(DevicePixels(16), DevicePixels(16)))
            .unwrap(),
    );
    window
        .update(cx, |showcase, window, _| {
            showcase.0 = Arc::downgrade(&replacement);
            replacement.render(window, |view| {
                clear(&other, view, wgpu::Color::BLUE);
                Ok(())
            })
        })
        .unwrap()
        .unwrap();
    window
        .update(cx, |_, window, cx| {
            window.draw(cx).clear(cx);
            let scale = window.scale_factor();
            let image = draw_other(
                &other,
                other_format,
                window.painted_scene(),
                (64. * scale) as u32,
            );
            pixel(&image, scale, 24., 40., [0, 0, 255, 255]);
        })
        .unwrap();
}
