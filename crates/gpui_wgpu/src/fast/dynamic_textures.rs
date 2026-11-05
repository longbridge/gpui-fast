//! GPU-written textures sampled directly by the ordinary sprite pipeline.
//!
//! The context owns the lookup table; the producer owns each entry through
//! `WgpuDynamicTexture`. No GPU objects enter GPUI's process-wide upload
//! backend, so browser handles stay on their browser thread too.

use std::{collections::HashMap, sync::Arc};

use anyhow::Result;
use gpui::{Bounds, DevicePixels, DynamicTexture, DynamicTextureId, Pixels, Size, Window};
use parking_lot::Mutex;

use crate::WgpuContext;

#[derive(Clone, Default)]
pub(crate) struct TextureStore(Arc<Mutex<HashMap<DynamicTextureId, wgpu::TextureView>>>);

impl TextureStore {
    pub(crate) fn view(&self, id: DynamicTextureId) -> Option<wgpu::TextureView> {
        self.0.lock().get(&id).cloned()
    }
}

/// An owned render target that GPUI samples without an upload or transfer copy.
///
/// Allocate it from the context drawing the window. Dropping it frees its
/// entry and retires its sprite id, so later sprite lookups skip it.
/// Notify the view when removing content, as for other view state changes.
/// A different context cannot resolve its id, even on the same adapter.
///
/// Check the creating context's `device_lost()` when handling device loss,
/// even if contents change rarely. Drop its targets promptly, before replacing
/// the context: each owner retains a context clone and its device. Recreate
/// targets from the replacement context and render their contents again.
/// `render` refuses a lost device; `paint` can still succeed for a stale owner,
/// but the replacement context skips its sprite and displays no texture.
pub struct WgpuDynamicTexture {
    context: WgpuContext,
    texture: DynamicTexture,
}

impl WgpuContext {
    /// Allocates a GPU-written dynamic texture on this context's device.
    ///
    /// The view uses `color_texture_format()` and `RENDER_ATTACHMENT |
    /// TEXTURE_BINDING`. There is no initial pixel upload. Render new contents
    /// with `WgpuDynamicTexture::render`, then paint with its `paint` method.
    pub fn create_dynamic_texture(&self, size: Size<DevicePixels>) -> Result<WgpuDynamicTexture> {
        anyhow::ensure!(!self.device_lost(), "dynamic texture device is lost");
        let limit = self.device.limits().max_texture_dimension_2d;
        anyhow::ensure!(
            size.width.0 > 0
                && size.height.0 > 0
                && size.width.0 as u32 <= limit
                && size.height.0 as u32 <= limit,
            "dynamic texture size must be positive and within the device's {limit} pixel limit"
        );
        let target = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("dynamic_texture"),
            size: wgpu::Extent3d {
                width: size.width.0 as u32,
                height: size.height.0 as u32,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.color_texture_format(),
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let texture = DynamicTexture::new_gpu(size)?;
        self.fast_dynamic_textures
            .0
            .lock()
            .insert(texture.id(), target.create_view(&Default::default()));
        Ok(WgpuDynamicTexture {
            context: self.clone(),
            texture,
        })
    }
}

impl WgpuDynamicTexture {
    /// Writes new contents and refreshes the window only after success.
    ///
    /// Submit the producer's commands on the creating context's queue before
    /// returning. GPUI's later submission samples the same texture; queue
    /// ordering makes a CPU wait or transfer copy unnecessary. Do not submit
    /// partial writes and then return an error. Call only when contents change.
    /// Outside paint, success requests a redraw. During paint it updates the
    /// current frame without scheduling another frame.
    /// Only this window is refreshed; notify other windows displaying the
    /// target so they redraw its new contents too.
    pub fn render<T>(
        &self,
        window: &mut Window,
        render: impl FnOnce(&wgpu::TextureView) -> Result<T>,
    ) -> Result<T> {
        anyhow::ensure!(
            !self.context.device_lost(),
            "dynamic texture device is lost"
        );
        let view = self
            .context
            .fast_dynamic_textures
            .view(self.texture.id())
            .ok_or_else(|| anyhow::anyhow!("dynamic texture was removed"))?;
        let result = render(&view)?;
        window.refresh_dynamic_texture(self.texture)?;
        Ok(result)
    }

    /// Paints with the window's current clipping, opacity and painter order.
    /// Call during paint in a window using the creating context.
    pub fn paint(&self, window: &mut Window, bounds: Bounds<Pixels>) -> Result<()> {
        window.paint_dynamic_texture(bounds, self.texture)
    }
}

impl Drop for WgpuDynamicTexture {
    fn drop(&mut self) {
        self.context
            .fast_dynamic_textures
            .0
            .lock()
            .remove(&self.texture.id());
        self.texture.remove();
    }
}

#[cfg(all(test, feature = "test-support", not(target_family = "wasm")))]
mod tests;
