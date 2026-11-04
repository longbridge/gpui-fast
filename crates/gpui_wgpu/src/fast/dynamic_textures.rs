//! Dynamic textures for the wgpu renderer.
//!
//! Each texture is its own `wgpu::Texture` outside the sprite atlas, in the
//! atlas's color format, usable as a render attachment: besides the core's
//! uploads, a producer on the renderer's device can render into it directly,
//! through [`WgpuContext::render_dynamic_texture`]. The frame resolves a
//! sprite of a dynamic texture through [`view`] instead of the atlas.
//!
//! wgpu gives a device one queue, and a queue runs writes and submissions in
//! the order they were made, so neither uploads nor a producer's passes need
//! staging or fences: a write submitted before the window draws is visible to
//! that frame, and one submitted after waits for the frames sampling the old
//! contents.
//!
//! Each texture stays on the device that created it. A weak registry lets a
//! frame find that device's textures without keeping unused devices alive.
//! Browsers are not supported: wgpu's objects are not `Send` there.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use anyhow::{Context as _, Result};
use gpui::{
    Bounds, DevicePixels, DynamicTexture, DynamicTextureId, DynamicTexturePlatform, Size,
    set_dynamic_texture_platform,
};
use parking_lot::{Mutex, RwLock};

use crate::{DeviceErrorState, WgpuContext};

/// Backends are owned by live handles and the default upload platform.
static BACKENDS: RwLock<Vec<Weak<WgpuDynamicTextures>>> = RwLock::new(Vec::new());

fn backend_for(context: &WgpuContext) -> Arc<WgpuDynamicTextures> {
    let mut backends = BACKENDS.write();
    backends.retain(|backend| backend.strong_count() != 0);
    for backend in backends.iter().filter_map(Weak::upgrade) {
        let matches = backend.0.lock().device.as_ref().get() == context.device.as_ref();
        if matches {
            return backend;
        }
    }
    let backend = Arc::new(WgpuDynamicTextures(Mutex::new(DeviceTextures::new(
        context,
    ))));
    backends.push(Arc::downgrade(&backend));
    backend
}

/// Installs the default upload backend when a WGPU window is created.
/// Explicit GPU creation chooses its context's backend independently.
pub(crate) fn init(context: &WgpuContext) {
    set_dynamic_texture_platform(backend_for(context));
}

/// Resolve only a texture belonging to the frame's device. Released handles
/// and textures on another device are skipped like released atlas entries.
pub(crate) fn view(device: &wgpu::Device, id: DynamicTextureId) -> Option<wgpu::TextureView> {
    let backends = BACKENDS.read();
    for backend in backends.iter().filter_map(Weak::upgrade) {
        let textures = backend.0.lock();
        if textures.device.as_ref().get() == device && !textures.errors.device_lost() {
            return textures
                .textures
                .get(&id)
                .map(|entry| entry.as_ref().get().clone());
        }
    }
    None
}

impl WgpuContext {
    /// Creates a GPU-written dynamic texture on this context's device.
    ///
    /// This selects the producer's backend explicitly, even if another
    /// renderer installed the process's default upload backend first. Paint
    /// the handle with `Window::paint_dynamic_texture` in a window using this
    /// context, and release it with `Window::remove_dynamic_texture`.
    pub fn create_dynamic_texture(&self, size: Size<DevicePixels>) -> Result<DynamicTexture> {
        anyhow::ensure!(!self.device_lost(), "dynamic texture device is lost");
        let backend = backend_for(self);
        DynamicTexture::new_gpu(size, backend)
    }

    /// Renders new contents into a dynamic texture and invalidates the window
    /// after the producer succeeds.
    ///
    /// The callback receives the texture in [`WgpuContext::color_texture_format`]
    /// with `RENDER_ATTACHMENT` usage. Submit the producer's commands on this
    /// context's queue before returning. The window's later submission then
    /// samples the new contents without a copy or CPU wait. An error from the
    /// producer is returned without recording a successful update; the callback
    /// must not submit partial writes and then report failure.
    pub fn render_dynamic_texture<T>(
        &self,
        window: &mut gpui::Window,
        texture: DynamicTexture,
        render: impl FnOnce(&wgpu::TextureView) -> Result<T>,
    ) -> Result<T> {
        let target = self.dynamic_texture(texture)?;
        let result = render(&target)?;
        window.refresh_dynamic_texture(texture)?;
        Ok(result)
    }

    /// Resolve the current handle on this device for each write, so removal
    /// or device replacement cannot silently select a retained old texture.
    fn dynamic_texture(&self, texture: DynamicTexture) -> Result<wgpu::TextureView> {
        anyhow::ensure!(!self.device_lost(), "dynamic texture device is lost");
        view(&self.device, texture.id())
            .context("dynamic texture was removed or belongs to another device")
    }
}

struct WgpuDynamicTextures(Mutex<DeviceTextures>);

/// The textures on the device the backend is installed on.
struct DeviceTextures {
    device: Arc<dyn Shared<wgpu::Device>>,
    queue: Arc<dyn Shared<wgpu::Queue>>,
    format: wgpu::TextureFormat,
    errors: Arc<DeviceErrorState>,
    textures: HashMap<DynamicTextureId, Box<dyn Shared<wgpu::TextureView>>>,
}

// Prove Send/Sync once at each native handle, rather than walking WGPU's
// deeply nested auto-trait graph again through the shared store's wrappers.
trait Shared<T>: Send + Sync {
    fn get(&self) -> &T;
}

impl<T: Send + Sync> Shared<T> for T {
    fn get(&self) -> &T {
        self
    }
}

impl DeviceTextures {
    fn new(context: &WgpuContext) -> Self {
        Self {
            device: context.device.clone(),
            queue: context.queue.clone(),
            format: context.color_texture_format(),
            errors: context.errors().clone(),
            textures: HashMap::new(),
        }
    }

    /// Allocates the texture for `id`. The size is checked here because wgpu
    /// reports an oversized texture to the device's error callback, which
    /// renderers treat as a failed frame, instead of to the caller.
    fn allocate(
        &mut self,
        id: DynamicTextureId,
        size: Size<DevicePixels>,
    ) -> Result<wgpu::TextureView> {
        let limit = self.device.as_ref().get().limits().max_texture_dimension_2d;
        let (width, height) = (size.width.0 as u32, size.height.0 as u32);
        anyhow::ensure!(
            width <= limit && height <= limit,
            "a {width}x{height} dynamic texture exceeds the device's {limit} pixel limit"
        );
        let texture = self
            .device
            .as_ref()
            .get()
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("dynamic_texture"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: self.format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_DST
                    | wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let entry = self.textures.entry(id).or_insert_with(|| Box::new(view));
        Ok(entry.as_ref().get().clone())
    }

    /// Writes tightly packed BGRA8 rows for `bounds` into `texture`. The write
    /// is queued and lands with the queue's next submission, the next frame's
    /// at the latest.
    fn write(&self, texture: &wgpu::Texture, bounds: Bounds<DevicePixels>, bytes: &[u8]) {
        let bytes = match self.format {
            wgpu::TextureFormat::Rgba8Unorm => std::borrow::Cow::Owned(bgra_to_rgba(bytes)),
            _ => std::borrow::Cow::Borrowed(bytes),
        };
        self.queue.as_ref().get().write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: bounds.origin.x.0 as u32,
                    y: bounds.origin.y.0 as u32,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            &bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bounds.size.width.0 as u32 * 4),
                rows_per_image: None,
            },
            wgpu::Extent3d {
                width: bounds.size.width.0 as u32,
                height: bounds.size.height.0 as u32,
                depth_or_array_layers: 1,
            },
        );
    }
}

/// The core hands over BGRA8; an RGBA8 atlas format stores it swapped, as the
/// sprite atlas's uploads do.
fn bgra_to_rgba(bytes: &[u8]) -> Vec<u8> {
    let mut data = bytes.to_vec();
    for pixel in data.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    data
}

impl DynamicTexturePlatform for WgpuDynamicTextures {
    fn create_texture(
        &self,
        id: DynamicTextureId,
        size: Size<DevicePixels>,
        bytes: &[u8],
    ) -> Result<()> {
        let mut textures = self.0.lock();
        let view = textures.allocate(id, size)?;
        let bounds = Bounds {
            origin: gpui::point(DevicePixels(0), DevicePixels(0)),
            size,
        };
        textures.write(view.texture(), bounds, bytes);
        Ok(())
    }

    fn update_texture(
        &self,
        id: DynamicTextureId,
        bounds: Bounds<DevicePixels>,
        bytes: &[u8],
    ) -> Result<()> {
        let textures = self.0.lock();
        let texture = textures
            .textures
            .get(&id)
            .with_context(|| format!("unknown dynamic texture {id:?}"))?
            .as_ref()
            .get()
            .texture();
        textures.write(texture, bounds, bytes);
        Ok(())
    }

    fn remove_texture(&self, id: DynamicTextureId) {
        self.0.lock().textures.remove(&id);
    }

    fn create_gpu_texture(&self, id: DynamicTextureId, size: Size<DevicePixels>) -> Result<()> {
        self.0.lock().allocate(id, size).map(|_| ())
    }
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use gpui::{
        AppContext as _, Bounds, Context, DevicePixels, DynamicTexture, IntoElement,
        ParentElement as _, PlatformHeadlessRenderer as _, Render, Styled as _, Window, canvas,
        div, point, px, size,
    };

    use crate::WgpuContext;
    use crate::wgpu_renderer::WgpuHeadlessRenderer;

    const RED: [u8; 4] = [255, 0, 0, 255];
    const BLUE: [u8; 4] = [0, 0, 255, 255];
    const BLACK: [u8; 4] = [0, 0, 0, 255];
    const WHITE: [u8; 4] = [255, 255, 255, 255];

    /// White, with the texture stretched over 64x64 inside a parent that
    /// clips it to the left half, and a black square painted over its top
    /// left corner after it.
    struct Showcase(DynamicTexture);

    impl Render for Showcase {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let texture = self.0;
            div()
                .relative()
                .size(px(64.))
                .bg(gpui::white())
                .child(
                    div().w(px(32.)).h(px(64.)).overflow_hidden().child(
                        canvas(
                            |_, _, _| (),
                            move |bounds, (), window, _| {
                                // A removed texture's paint is refused; the
                                // frame then draws nothing in its place.
                                let _ = window.paint_dynamic_texture(bounds, texture);
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

    /// Renders `color` into the whole texture as a producer would: a render
    /// pass on the context's device, submitted on its queue.
    fn render_into(context: &WgpuContext, view: &wgpu::TextureView, color: wgpu::Color) {
        let mut encoder = context
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("producer"),
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

    /// The pixel at `(x, y)` in 64x64 window coordinates, with a small
    /// tolerance for the drivers' filtering.
    fn assert_pixel(image: &image::RgbaImage, scale: f32, (x, y): (f32, f32), expected: [u8; 4]) {
        let actual = image.get_pixel((x * scale) as u32, (y * scale) as u32).0;
        assert!(
            actual
                .iter()
                .zip(expected)
                .all(|(actual, expected)| actual.abs_diff(expected) <= 3),
            "pixel ({x}, {y}) was {actual:?}, expected {expected:?}"
        );
    }

    /// A texture a producer renders into on the renderer's device is drawn by
    /// the sprite pipeline under its parent's clip and below what is painted
    /// after it; uploads land on top of the producer's contents, and a removed
    /// texture is refused to the producer and no longer drawn.
    #[gpui::test]
    fn a_gpu_written_texture_draws_as_a_clipped_ordered_sprite(cx: &mut gpui::TestAppContext) {
        let (context, _) = WgpuContext::new_headless().expect("dynamic texture tests need a GPU");
        super::init(&context);
        let mut renderer = WgpuHeadlessRenderer::new().expect("headless renderer");

        let window = cx.update(|cx| {
            cx.open_window(Default::default(), |_, cx| {
                let texture = context
                    .create_dynamic_texture(size(DevicePixels(16), DevicePixels(16)))
                    .expect("the wgpu backend creates GPU-written textures");
                cx.new(|_| Showcase(texture))
            })
            .unwrap()
        });
        let texture = window.update(cx, |showcase, _, _| showcase.0).unwrap();

        let mut draw = |cx: &mut gpui::TestAppContext| {
            window
                .update(cx, |_, window, cx| {
                    window.draw(cx).clear(cx);
                    let scale = window.scale_factor();
                    let side = DevicePixels((64. * scale) as i32);
                    let image = renderer
                        .render_scene_to_image(window.painted_scene(), size(side, side))
                        .expect("frame rendered");
                    (image, scale)
                })
                .unwrap()
        };

        window
            .update(cx, |_, window, _| {
                context.render_dynamic_texture(window, texture, |target| {
                    assert_eq!(target.texture().format(), context.color_texture_format());
                    render_into(&context, target, wgpu::Color::RED);
                    Ok(())
                })
            })
            .unwrap()
            .expect("live texture");
        let (image, scale) = draw(cx);
        assert_pixel(&image, scale, (24., 40.), RED);
        assert_pixel(&image, scale, (8., 8.), BLACK);
        assert_pixel(&image, scale, (48., 40.), WHITE);

        // The top-left 8x8 texels cover window pixels 0..32 of each axis.
        window
            .update(cx, |_, window, _| {
                let blue = [255, 0, 0, 255].repeat(8 * 8);
                window.update_dynamic_texture(
                    texture,
                    Bounds {
                        origin: point(DevicePixels(0), DevicePixels(0)),
                        size: size(DevicePixels(8), DevicePixels(8)),
                    },
                    &blue,
                    8 * 4,
                )
            })
            .unwrap()
            .expect("an upload into a GPU-written texture");
        let (image, scale) = draw(cx);
        assert_pixel(&image, scale, (24., 24.), BLUE);
        assert_pixel(&image, scale, (24., 56.), RED);

        window
            .update(cx, |_, window, _| window.remove_dynamic_texture(texture))
            .unwrap();
        assert!(
            window
                .update(cx, |_, window, _| {
                    context.render_dynamic_texture::<()>(window, texture, |_| {
                        unreachable!("removed texture")
                    })
                })
                .unwrap()
                .is_err(),
            "a removed texture must be refused to the producer"
        );
        let (image, scale) = draw(cx);
        assert_pixel(&image, scale, (24., 40.), WHITE);
    }
}
