//! Choosing the window surface's texture format.

/// The two formats the renderer draws a window into.
const DRAWABLE: [wgpu::TextureFormat; 2] = [
    wgpu::TextureFormat::Bgra8Unorm,
    wgpu::TextureFormat::Rgba8Unorm,
];

/// The browser's preferred canvas format, when the renderer can draw into it.
///
/// Upstream takes `Bgra8Unorm` whenever the surface lists it. In a browser
/// that is not always the device's preferred canvas format: on many Android
/// and Linux devices `navigator.gpu.getPreferredCanvasFormat()` is
/// `rgba8unorm`, and configuring the canvas with the other format makes the
/// browser copy every frame to convert it (Chrome logs "WebGPU canvas
/// configured with a different format than is preferred by this device").
/// wgpu's WebGPU backend reports the preferred format first, so on the web
/// this returns it; elsewhere it returns `None` and upstream's order applies.
pub(crate) fn preferred_canvas(formats: &[wgpu::TextureFormat]) -> Option<wgpu::TextureFormat> {
    preferred_canvas_for(formats, cfg!(target_family = "wasm"))
}

fn preferred_canvas_for(
    formats: &[wgpu::TextureFormat],
    first_is_preferred: bool,
) -> Option<wgpu::TextureFormat> {
    formats
        .first()
        .copied()
        .filter(|first| first_is_preferred && DRAWABLE.contains(first))
}

#[cfg(test)]
mod tests {
    use super::preferred_canvas_for;
    use wgpu::TextureFormat::{Bgra8Unorm, Rgba8Unorm, Rgba16Float};

    #[test]
    fn web_takes_the_preferred_canvas_format() {
        // A device whose preferred canvas format is `rgba8unorm`.
        assert_eq!(
            preferred_canvas_for(&[Rgba8Unorm, Bgra8Unorm, Rgba16Float], true),
            Some(Rgba8Unorm)
        );
        assert_eq!(
            preferred_canvas_for(&[Bgra8Unorm, Rgba8Unorm, Rgba16Float], true),
            Some(Bgra8Unorm)
        );
    }

    #[test]
    fn web_skips_a_preferred_format_the_renderer_does_not_draw_into() {
        assert_eq!(
            preferred_canvas_for(&[Rgba16Float, Rgba8Unorm, Bgra8Unorm], true),
            None
        );
        assert_eq!(preferred_canvas_for(&[], true), None);
    }

    #[test]
    fn native_leaves_the_choice_to_upstream_order() {
        assert_eq!(preferred_canvas_for(&[Rgba8Unorm, Bgra8Unorm], false), None);
    }
}
