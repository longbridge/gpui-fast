use std::sync::{Arc, Weak};

use gpui::{
    AppContext as _, Bounds, Context, DevicePixels, DynamicTexture, IntoElement,
    ParentElement as _, PlatformHeadlessRenderer as _, Render, Styled as _, Window, canvas, div,
    point, px, size,
};

use super::{
    DirtySet, GLOBAL_EPOCH, MAX_DIRTY_RECTS, MAX_STAGING_BYTES, MIN_STAGING_BYTES, PLATFORM,
    STAGING_IDLE_UPLOADS, TextureStaging, bounds_area, create_upload_texture, handle_device_lost,
    init, release_lost_device, swizzle_subregion_bgra_to_rgba,
};
use crate::wgpu_renderer::WgpuRendererCore;
use crate::{WgpuAtlas, WgpuContext, WgpuDynamicTexture, WgpuHeadlessRenderer};

/// Serializes tests that exercise the process-wide CPU-upload `PLATFORM`.
static UPLOAD_PLATFORM_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

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

struct UploadedShowcase(Option<DynamicTexture>);

impl Render for UploadedShowcase {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let texture = self.0;
        div().size(px(64.)).bg(gpui::white()).child(
            canvas(
                |_, _, _| (),
                move |bounds, (), window, _| {
                    if let Some(texture) = texture {
                        window.paint_dynamic_texture(bounds, texture).unwrap();
                    }
                },
            )
            .size(px(64.)),
        )
    }
}

fn strided_bgra(width: usize, height: usize, pad_bytes: usize, bgra: [u8; 4]) -> (Vec<u8>, u32) {
    let stride = width * 4 + pad_bytes;
    let mut bytes = vec![0xAA; stride * height];
    for row in 0..height {
        let start = row * stride;
        for px_chunk in bytes[start..start + width * 4].as_chunks_mut::<4>().0 {
            *px_chunk = bgra;
        }
    }
    (bytes, stride as u32)
}

#[gpui::test]
fn uploaded_textures_update_subregions_and_stay_context_local(cx: &mut gpui::TestAppContext) {
    let _guard = UPLOAD_PLATFORM_LOCK.lock();
    let (headless_ctx, _) = WgpuContext::new_headless().expect("GPU required");
    let mut renderer = WgpuHeadlessRenderer::new().expect("GPU required");
    handle_device_lost(&headless_ctx);
    let (other, other_format) = WgpuContext::create_headless().expect("second GPU device");
    assert!(!Arc::ptr_eq(&other.device, &renderer.core.resources.device));

    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| {
            cx.new(|_| UploadedShowcase(None))
        })
        .unwrap()
    });

    // BGRA [255, 0, 0, 255] -> rendered RGBA [0, 0, 255, 255] (blue), with
    // non-zero row padding to exercise stride stripping end-to-end.
    let (initial_blue, initial_stride) = strided_bgra(16, 16, 12, [255, 0, 0, 255]);
    let texture = window
        .update(cx, |showcase, window, cx| {
            let texture = window
                .create_dynamic_texture(
                    size(DevicePixels(16), DevicePixels(16)),
                    &initial_blue,
                    initial_stride,
                )
                .expect("create uploaded dynamic texture");
            showcase.0 = Some(texture);
            cx.notify();
            texture
        })
        .unwrap();

    cx.update_window(window.into(), |_, window, cx| {
        let mut primary_epoch = headless_ctx.errors().current_generation();
        let mut other_epoch = other.errors().current_generation();

        window.draw(cx).clear(cx);
        let scale = window.scale_factor();
        let side = DevicePixels((64. * scale) as i32);
        let image = renderer
            .render_scene_to_image(window.painted_scene(), size(side, side))
            .unwrap();
        pixel(&image, scale, 24., 20., [0, 0, 255, 255]);

        // Sub-region update on the bottom half + interior 4x4 patch.
        let (bottom_red, bottom_stride) = strided_bgra(16, 8, 8, [0, 0, 255, 255]);
        window
            .update_dynamic_texture(
                texture,
                Bounds {
                    origin: point(DevicePixels(0), DevicePixels(8)),
                    size: size(DevicePixels(16), DevicePixels(8)),
                },
                &bottom_red,
                bottom_stride,
            )
            .expect("update bottom half");

        let (patch_green, patch_stride) = strided_bgra(4, 4, 4, [0, 255, 0, 255]);
        window
            .update_dynamic_texture(
                texture,
                Bounds {
                    origin: point(DevicePixels(4), DevicePixels(4)),
                    size: size(DevicePixels(4), DevicePixels(4)),
                },
                &patch_green,
                patch_stride,
            )
            .expect("update interior patch");

        window.draw(cx).clear(cx);
        let updated = renderer
            .render_scene_to_image(window.painted_scene(), size(side, side))
            .unwrap();
        pixel(&updated, scale, 6., 22., [0, 0, 255, 255]);
        pixel(&updated, scale, 24., 24., [0, 255, 0, 255]);
        pixel(&updated, scale, 24., 48., [255, 0, 0, 255]);
        assert!(
            headless_ctx
                .errors()
                .observe_error(&mut primary_epoch)
                .is_none()
        );

        // An independent foreign context skips the uploaded sprite cleanly
        // without validation errors, matching `frame.rs`.
        let foreign = draw_other(&other, other_format, window.painted_scene(), side.0 as u32);
        pixel(&foreign, scale, 24., 24., [255, 255, 255, 255]);
        assert!(other.errors().observe_error(&mut other_epoch).is_none());

        // Removing the texture retires it even if a cached scene replays without redrawing,
        // and advances `synced_epoch` so the next `before_frame` stays on the lock-free fast path.
        window.remove_dynamic_texture(texture);
        assert_eq!(
            headless_ctx
                .fast_dynamic_textures
                .0
                .synced_epoch
                .load(std::sync::atomic::Ordering::Acquire),
            GLOBAL_EPOCH.load(std::sync::atomic::Ordering::Acquire),
        );
        let removed = renderer
            .render_scene_to_image(window.painted_scene(), size(side, side))
            .unwrap();
        pixel(&removed, scale, 24., 24., [255, 255, 255, 255]);
    })
    .unwrap();
}

#[gpui::test]
fn uploaded_textures_preserve_paused_and_partial_post_recovery_pixels(
    cx: &mut gpui::TestAppContext,
) {
    let _guard = UPLOAD_PLATFORM_LOCK.lock();
    let (old, old_format) = WgpuContext::create_headless().unwrap();
    init(&old);
    handle_device_lost(&old);

    let window = cx.update(|cx| {
        cx.open_window(Default::default(), |_, cx| {
            cx.new(|_| UploadedShowcase(None))
        })
        .unwrap()
    });
    let dimensions = size(DevicePixels(1536), DevicePixels(1024));
    let full = Bounds {
        origin: point(DevicePixels(0), DevicePixels(0)),
        size: dimensions,
    };
    let (initial, stride) = strided_bgra(1536, 1024, 0, [0, 255, 0, 255]);
    let (blue, _) = strided_bgra(1536, 1024, 0, [255, 0, 0, 255]);
    let (yellow, _) = strided_bgra(1536, 1024, 0, [0, 255, 255, 255]);
    let texture = window
        .update(cx, |showcase, window, cx| {
            let texture = window
                .create_dynamic_texture(dimensions, &initial, stride)
                .unwrap();
            showcase.0 = Some(texture);
            cx.notify();
            texture
        })
        .unwrap();
    old.fast_dynamic_textures.before_frame();
    cx.update_window(window.into(), |_, window, cx| {
        window
            .update_dynamic_texture(texture, full, &blue, stride)
            .unwrap();
        window.draw(cx).clear(cx);
        let scale = window.scale_factor();
        let before = draw_other(
            &old,
            old_format,
            window.painted_scene(),
            (64. * scale) as u32,
        );
        pixel(&before, scale, 24., 20., [0, 0, 255, 255]);
        window
            .update_dynamic_texture(texture, full, &yellow, stride)
            .unwrap();
    })
    .unwrap();
    let old_buffers = {
        let state = PLATFORM.get().unwrap().state.lock();
        let buffers = &state.gpu.as_ref().unwrap().staging.buffers;
        assert_eq!(buffers.lock().ready.len(), 1);
        Arc::downgrade(buffers)
    };
    old.fast_dynamic_textures.before_frame();
    assert_eq!(
        old_buffers.upgrade().unwrap().lock().allocated,
        MIN_STAGING_BYTES
    );

    old.device.destroy();
    release_lost_device();
    assert!(old_buffers.upgrade().is_none());
    let (replacement, format) = WgpuContext::create_headless().unwrap();
    handle_device_lost(&replacement);
    assert!(old.fast_dynamic_textures.view(texture.id()).is_none());
    cx.update_window(window.into(), |_, window, _| {
        let scale = window.scale_factor();
        let side = (64. * scale) as u32;
        let mut epoch = replacement.errors().current_generation();
        let paused = draw_other(&replacement, format, window.painted_scene(), side);
        pixel(&paused, scale, 24., 20., [255, 255, 0, 255]);
        pixel(&paused, scale, 24., 48., [255, 255, 0, 255]);
        assert!(replacement.errors().observe_error(&mut epoch).is_none());
    })
    .unwrap();

    release_lost_device();
    let (headless_ctx, _) = WgpuContext::new_headless().unwrap();
    handle_device_lost(&headless_ctx);
    let mut renderer = WgpuHeadlessRenderer::new().unwrap();
    let (bottom_red, stride) = strided_bgra(1536, 512, 0, [0, 0, 255, 255]);
    window
        .update(cx, |_, window, _| {
            window
                .update_dynamic_texture(
                    texture,
                    Bounds {
                        origin: point(DevicePixels(0), DevicePixels(512)),
                        size: size(DevicePixels(1536), DevicePixels(512)),
                    },
                    &bottom_red,
                    stride,
                )
                .unwrap();
        })
        .unwrap();
    cx.update_window(window.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        let scale = window.scale_factor();
        let side = DevicePixels((64. * scale) as i32);
        let partial = renderer
            .render_scene_to_image(window.painted_scene(), size(side, side))
            .unwrap();
        pixel(&partial, scale, 24., 20., [255, 255, 0, 255]);
        pixel(&partial, scale, 24., 48., [255, 0, 0, 255]);

        window
            .update_dynamic_texture(texture, full, &blue, stride)
            .unwrap();
        window.draw(cx).clear(cx);
        let resumed = renderer
            .render_scene_to_image(window.painted_scene(), size(side, side))
            .unwrap();
        pixel(&resumed, scale, 24., 48., [0, 0, 255, 255]);
        {
            let state = PLATFORM.get().unwrap().state.lock();
            let buffers = &state.gpu.as_ref().unwrap().staging.buffers;
            assert_eq!(buffers.lock().allocated, MIN_STAGING_BYTES);
        }
        window.remove_dynamic_texture(texture);
    })
    .unwrap();
}

#[test]
fn staging_buffers_are_bounded_reused_and_released() {
    let (context, _) = WgpuContext::new_headless().unwrap();
    let extent = wgpu::Extent3d {
        width: 1536,
        height: 1024,
        depth_or_array_layers: 1,
    };
    let texture = create_upload_texture(
        &context.device,
        context.color_texture_format(),
        size(
            DevicePixels(extent.width as i32),
            DevicePixels(extent.height as i32),
        ),
    );
    let pixels = [65, 80, 95, 255].repeat((extent.width * extent.height) as usize);
    let mut staging = TextureStaging::default();
    let upload = |staging: &mut TextureStaging| {
        assert!(staging.upload(
            &context.device,
            texture.as_image_copy(),
            extent,
            &pixels,
            extent.width * 4,
            context.device.limits().max_buffer_size,
        ));
        let mut buffers = staging.buffers.lock();
        assert_eq!(buffers.allocated, MIN_STAGING_BYTES);
        assert!(buffers.reserve(MAX_STAGING_BYTES).is_none());
    };

    upload(&mut staging);
    context.queue.submit([staging.finish_uploads().unwrap()]);
    context
        .device
        .poll(wgpu::PollType::wait_indefinitely())
        .unwrap();
    assert_eq!(staging.buffers.lock().ready.len(), 1);

    upload(&mut staging);
    context.queue.submit([staging.finish_uploads().unwrap()]);
    for _ in 0..STAGING_IDLE_UPLOADS {
        assert!(staging.finish_uploads().is_none());
    }
    context
        .device
        .poll(wgpu::PollType::wait_indefinitely())
        .unwrap();
    let buffers = staging.buffers.lock();
    assert!(buffers.ready.is_empty());
    assert_eq!(buffers.allocated, 0);
}

#[test]
fn dirty_set_coalesces_repeated_and_overflowing_updates_without_fullscreen_fallback() {
    let quarter = Bounds {
        origin: point(DevicePixels(0), DevicePixels(0)),
        size: size(DevicePixels(960), DevicePixels(540)),
    };
    let mut repeated = DirtySet::new();
    for _ in 0..20 {
        repeated.push(quarter);
    }
    let (regions, count) = repeated.take();
    assert_eq!(&regions[..count], &[quarter]);
    assert_eq!(repeated.take().1, 0);

    let updates = (0..10).flat_map(|column| {
        [0, 1000].map(move |row| Bounds {
            origin: point(DevicePixels(column * 20), DevicePixels(row)),
            size: size(DevicePixels(8), DevicePixels(16)),
        })
    });
    let mut dirty = DirtySet::new();
    for update in updates.clone() {
        dirty.push(update);
    }
    let (regions, count) = dirty.take();
    let regions = &regions[..count];
    assert!((2..=MAX_DIRTY_RECTS).contains(&count));
    assert!(regions.iter().copied().map(bounds_area).sum::<u64>() <= 2 * 188 * 16);
    for update in updates {
        assert!(
            regions
                .iter()
                .any(|region| region.intersect(&update) == update)
        );
    }
}

#[test]
fn swizzle_subregion_bgra_to_rgba_converts_subrects_and_reuses_scratch_buffer() {
    let mut scratch = Vec::new();
    let cpu_pixels = [
        0x10, 0x20, 0x30, 0x40, 0x11, 0x22, 0x33, 0x44, //
        0x50, 0x60, 0x70, 0x80, 0xAA, 0xBB, 0xCC, 0xDD,
    ];
    assert_eq!(
        swizzle_subregion_bgra_to_rgba(&cpu_pixels, 8, 0, 0, 8, 2, &mut scratch),
        &[
            0x30, 0x20, 0x10, 0x40, 0x33, 0x22, 0x11, 0x44, //
            0x70, 0x60, 0x50, 0x80, 0xCC, 0xBB, 0xAA, 0xDD,
        ]
    );
    let ptr = scratch.as_ptr();
    let capacity = scratch.capacity();

    assert_eq!(
        swizzle_subregion_bgra_to_rgba(&cpu_pixels, 8, 1, 1, 4, 1, &mut scratch),
        &[0xCC, 0xBB, 0xAA, 0xDD]
    );
    assert_eq!(scratch.as_ptr(), ptr);
    assert_eq!(scratch.capacity(), capacity);
}
