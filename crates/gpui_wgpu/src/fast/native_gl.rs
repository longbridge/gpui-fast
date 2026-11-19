//! Native OpenGL adapters without compute shaders or vertex storage buffers.
//!
//! Some Linux machines have no Vulkan driver and only a GLES 3.0 class GL
//! context: Intel HD Graphics 2000/3000 (Sandy Bridge) with Mesa's `crocus`
//! driver, for example. Two things kept such an adapter from ever rendering,
//! so gpui fell back to llvmpipe and drew every frame on the CPU:
//!
//! - The device was requested with `Limits::downlevel_defaults()`, which asks
//!   for compute workgroups and storage buffers. The adapter has neither, so
//!   `request_device` failed and adapter selection moved on to llvmpipe.
//! - Instance data went through storage buffers, which a GLES 3.0 vertex
//!   shader can't read; every scene failed with "maximum of 0".
//!
//! WebGL2 has the same limits, and the renderer already has a texture-based
//! instance transport for it. These helpers request the limits such an adapter
//! actually has and route its instances through that same transport. Adapters
//! that meet `downlevel_defaults()` and have vertex storage keep the exact
//! behavior they had.

/// The limits to request from a native adapter.
pub(crate) fn required_limits(adapter: &wgpu::Adapter) -> wgpu::Limits {
    let limits = wgpu::Limits::downlevel_defaults()
        .using_resolution(adapter.limits())
        .using_alignment(adapter.limits());
    if adapter.get_info().backend != wgpu::Backend::Gl || limits.check_limits(&adapter.limits()) {
        return limits;
    }
    log::info!(
        "GL adapter {:?} is below downlevel_defaults (no compute); requesting its own limits",
        adapter.get_info().name
    );
    adapter.limits()
}

/// Whether a native adapter needs instance data in textures rather than in
/// storage buffers, because its vertex shaders can't read storage buffers.
pub(crate) fn uses_texture_instance_data(adapter: &wgpu::Adapter) -> bool {
    adapter.get_info().backend == wgpu::Backend::Gl
        && !adapter
            .get_downlevel_capabilities()
            .flags
            .contains(wgpu::DownlevelFlags::VERTEX_STORAGE)
}
