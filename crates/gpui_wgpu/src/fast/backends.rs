//! Which native wgpu backends an instance initializes.
//!
//! wgpu initializes every backend in an instance's set as soon as the instance
//! is created, whether or not an adapter from it is ever used. Upstream asks for
//! `VULKAN | GL`, and on Linux the GL backend loads glvnd's `libEGL`, which in
//! turn loads every installed vendor's EGL driver. On a machine that renders
//! through NVIDIA's Vulkan driver this still maps Mesa's `libgallium` and
//! `libLLVM` (tens of megabytes) into the process for an adapter that adapter
//! selection then ranks last.
//!
//! So a Vulkan-only instance is created first, and GL is added only when it
//! finds no adapter. `WGPU_BACKEND` (e.g. `WGPU_BACKEND=vulkan,gl`) overrides
//! the choice.

use std::sync::Arc;

use raw_window_handle::{DisplayHandle, HandleError, HasDisplayHandle};

/// A display handle shared between the instances tried here, since an
/// instance takes ownership of the one it is given.
#[derive(Debug, Clone)]
struct SharedDisplay(Arc<dyn wgpu::wgt::WgpuHasDisplayHandle>);

impl HasDisplayHandle for SharedDisplay {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        self.0.display_handle()
    }
}

pub(crate) fn native_instance(
    display: Option<Box<dyn wgpu::wgt::WgpuHasDisplayHandle>>,
) -> wgpu::Instance {
    if let Some(backends) = wgpu::Backends::from_env() {
        return create(backends, display);
    }

    let display = display.map(|display| SharedDisplay(Arc::from(display)));
    let vulkan = create(wgpu::Backends::VULKAN, boxed(&display));
    if !gpui::block_on(vulkan.enumerate_adapters(wgpu::Backends::VULKAN)).is_empty() {
        return vulkan;
    }
    log::info!("No Vulkan adapter found, adding the OpenGL backend");
    drop(vulkan);
    create(wgpu::Backends::VULKAN | wgpu::Backends::GL, boxed(&display))
}

fn boxed(display: &Option<SharedDisplay>) -> Option<Box<dyn wgpu::wgt::WgpuHasDisplayHandle>> {
    display
        .clone()
        .map(|display| Box::new(display) as Box<dyn wgpu::wgt::WgpuHasDisplayHandle>)
}

fn create(
    backends: wgpu::Backends,
    display: Option<Box<dyn wgpu::wgt::WgpuHasDisplayHandle>>,
) -> wgpu::Instance {
    wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends,
        flags: wgpu::InstanceFlags::default(),
        backend_options: wgpu::BackendOptions::default(),
        memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
        display,
    })
}
