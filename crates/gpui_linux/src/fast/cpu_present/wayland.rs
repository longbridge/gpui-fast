//! Presenting CPU frames on Wayland through `wl_shm` buffers.
//!
//! The frames are shown on a synchronized subsurface covering the window
//! surface, not on the window surface itself: the GPU's Vulkan WSI can give the
//! window surface explicit synchronization (`wp_linux_drm_syncobj_surface_v1`,
//! as NVIDIA's and recent Mesa drivers do), and committing a `wl_shm` buffer to
//! such a surface is a protocol error. A CPU frame attaches its buffer to the
//! subsurface and commits the window surface without a buffer, which carries
//! the frame callback the window requested and applies the subsurface's state.
//! When the GPU presents again the subsurface's buffer is removed, and the
//! WSI's commit of the window surface applies that with the GPU frame.
//!
//! The presenter keeps up to [`MAX_BUFFERS`] buffers the size of the frame,
//! each in its own `memfd`. A buffer the compositor still holds (attached and
//! not yet released with `wl_buffer.release`) is never written. Every buffer
//! remembers the regions it misses relative to the latest frame presented (the
//! damage of the frames presented since it was last attached), so a frame
//! copies only those and its own damage into the buffer it attaches.
//!
//! Where the compositor makes the window translucent (a window-opacity rule),
//! the window surface's last GPU frame would show through the subsurface, and
//! the two would make the window more opaque than the rule: a CPU frame
//! therefore makes the window surface fully transparent with
//! `wp_alpha_modifier_v1`, in the same commit, and the GPU frame after it
//! makes it opaque again in its own. Without that protocol there is no
//! presenter, and the renderer shows CPU frames by blit.
//!
//! The subsurface is scaled the way the window surface is: to the window's
//! logical size through a viewport, or by the buffer scale without
//! viewporter, so a buffer of the frame's size in device pixels shows the
//! same way the swapchain's images do. The window reports its size and scale
//! through [`window_resized`].

use std::{
    cell::RefCell,
    os::fd::AsFd as _,
    sync::{Arc, Mutex},
};

use anyhow::{Context as _, anyhow};
use collections::HashMap;
use gpui::{Bounds, DevicePixels, Size};
use gpui_wgpu::{CpuFrame, CpuPresenter, WgpuRenderer};
use wayland_backend::client::ObjectId;
use wayland_client::delegate_noop;
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    protocol::{wl_buffer, wl_shm, wl_subsurface, wl_surface},
};
use wayland_protocols::wp::alpha_modifier::v1::client::{
    wp_alpha_modifier_surface_v1, wp_alpha_modifier_v1,
};
use wayland_protocols::wp::viewporter::client::wp_viewport;

use super::memfd::Mapping;
use super::region::Region;
use crate::linux::{Globals, WaylandClientStatePtr};

/// The most buffers a presenter keeps: one shown, one the compositor may still
/// be reading, one to draw into, and one more for a compositor slow to release.
const MAX_BUFFERS: usize = 4;

type WindowScale = Option<(Size<DevicePixels>, f32)>;

/// `wp_alpha_modifier_v1`, when the compositor has it.
pub(crate) type AlphaModifier = Option<wp_alpha_modifier_v1::WpAlphaModifierV1>;

delegate_noop!(WaylandClientStatePtr: ignore wp_alpha_modifier_v1::WpAlphaModifierV1);
delegate_noop!(WaylandClientStatePtr: ignore wp_alpha_modifier_surface_v1::WpAlphaModifierSurfaceV1);

thread_local! {
    /// The size, in device pixels, and scale of each window surface with a
    /// presenter, once the window reported them.
    static WINDOW_SCALES: RefCell<HashMap<ObjectId, WindowScale>> =
        RefCell::new(HashMap::default());
}

/// `renderer`, letting it present the frames it draws on the CPU on
/// `surface`.
pub(crate) fn with_presenter(
    mut renderer: WgpuRenderer,
    globals: &Globals,
    surface: &wl_surface::WlSurface,
) -> WgpuRenderer {
    if globals.fast_subcompositor.is_none() {
        log::info!("CPU frames are not presented: no wl_subcompositor");
        return renderer;
    }
    if globals.fast_alpha_modifier.is_none() {
        log::info!("CPU frames are not presented: no wp_alpha_modifier_v1");
        return renderer;
    }
    WINDOW_SCALES.with_borrow_mut(|scales| scales.insert(surface.id(), None));
    renderer.set_cpu_presenter(Box::new(ShmPresenter {
        window_surface: surface.clone(),
        globals: globals.clone(),
        subsurface: None,
        window_alpha: None,
        shown: false,
        window_has_buffer: false,
        scaled_for: None,
        buffers: Vec::new(),
    }));
    renderer
}

/// Notes the size, in device pixels, and the scale of the window whose
/// surface is `window_surface`.
pub(crate) fn window_resized(
    window_surface: &wl_surface::WlSurface,
    size: Size<DevicePixels>,
    scale: f32,
) {
    WINDOW_SCALES.with_borrow_mut(|scales| {
        if let Some(entry) = scales.get_mut(&window_surface.id()) {
            *entry = Some((size, scale));
        }
    });
}

struct ShmPresenter {
    window_surface: wl_surface::WlSurface,
    globals: Globals,
    subsurface: Option<Subsurface>,
    /// The window surface's alpha multiplier, made with the subsurface.
    window_alpha: Option<wp_alpha_modifier_surface_v1::WpAlphaModifierSurfaceV1>,
    /// Whether the subsurface shows a buffer, or will once the window surface
    /// is committed.
    shown: bool,
    /// Whether the GPU presented a frame on the window surface. Until it has,
    /// the window surface has no buffer and is not mapped, and neither is the
    /// subsurface.
    window_has_buffer: bool,
    /// The window size and scale the subsurface is scaled for.
    scaled_for: WindowScale,
    buffers: Vec<ShmBuffer>,
}

/// The subsurface CPU frames are shown on.
struct Subsurface {
    surface: wl_surface::WlSurface,
    subsurface: wl_subsurface::WlSubsurface,
    viewport: Option<wp_viewport::WpViewport>,
}

impl Subsurface {
    fn new(globals: &Globals, parent: &wl_surface::WlSurface) -> anyhow::Result<Self> {
        let subcompositor = globals
            .fast_subcompositor
            .as_ref()
            .context("no wl_subcompositor")?;
        let surface = globals.compositor.create_surface(&globals.qh, ());
        let subsurface = subcompositor.get_subsurface(&surface, parent, &globals.qh, ());
        subsurface.set_position(0, 0);
        // Pointer input goes through to the window surface below.
        let region = globals.compositor.create_region(&globals.qh, ());
        surface.set_input_region(Some(&region));
        region.destroy();
        let viewport = globals
            .viewporter
            .as_ref()
            .map(|viewporter| viewporter.get_viewport(&surface, &globals.qh, ()));
        Ok(Self {
            surface,
            subsurface,
            viewport,
        })
    }
}

impl Drop for Subsurface {
    fn drop(&mut self) {
        // The viewport must be destroyed before its wl_surface.
        if let Some(viewport) = self.viewport.take() {
            viewport.destroy();
        }
        self.subsurface.destroy();
        self.surface.destroy();
    }
}

impl ShmPresenter {
    /// Scales the subsurface the way the window surface is, if the window
    /// reported a size or scale it is not scaled for yet.
    fn scale_subsurface(&mut self) {
        let Some(subsurface) = &self.subsurface else {
            return;
        };
        let reported = WINDOW_SCALES
            .with_borrow(|scales| scales.get(&self.window_surface.id()).copied().flatten());
        if reported.is_none() || reported == self.scaled_for {
            return;
        }
        self.scaled_for = reported;
        let Some((size, scale)) = reported else {
            return;
        };
        match &subsurface.viewport {
            Some(viewport) => {
                let width = (size.width.0 as f32 / scale).round() as i32;
                let height = (size.height.0 as f32 / scale).round() as i32;
                viewport.set_destination(width.max(1), height.max(1));
            }
            None => subsurface
                .surface
                .set_buffer_scale(scale.round().max(1.) as i32),
        }
    }
}

impl CpuPresenter for ShmPresenter {
    fn present(&mut self, frame: CpuFrame<'_>) -> anyhow::Result<()> {
        let (width, height) = (frame.width, frame.height);
        anyhow::ensure!(width > 0 && height > 0, "empty CPU frame");
        anyhow::ensure!(
            frame.pixels.len() >= width as usize * height as usize,
            "CPU frame has fewer pixels than its size"
        );
        anyhow::ensure!(
            self.window_has_buffer,
            "the window's first frame is to be presented by the GPU"
        );
        let format = if frame.opaque {
            wl_shm::Format::Xrgb8888
        } else {
            wl_shm::Format::Argb8888
        };

        // Buffers of another size or format are dropped, once the compositor
        // releases them.
        self.buffers
            .retain(|buffer| buffer.matches(width, height, format));

        let damage = Region::from_rects(frame.damage, width, height);
        for buffer in &mut self.buffers {
            buffer.missing.add(&damage);
        }

        let index = match self.buffers.iter().position(|buffer| !buffer.is_busy()) {
            Some(index) => index,
            None if self.buffers.len() < MAX_BUFFERS => {
                self.buffers.push(ShmBuffer::new(
                    &self.globals.shm,
                    &self.globals.qh,
                    width,
                    height,
                    format,
                )?);
                self.buffers.len() - 1
            }
            None => return Err(anyhow!("the compositor holds every wl_shm buffer")),
        };

        if self.subsurface.is_none() {
            self.subsurface = Some(Subsurface::new(&self.globals, &self.window_surface)?);
            self.window_alpha =
                self.globals.fast_alpha_modifier.as_ref().map(|modifier| {
                    modifier.get_surface(&self.window_surface, &self.globals.qh, ())
                });
        }
        self.scale_subsurface();
        let Some(subsurface) = &self.subsurface else {
            return Err(anyhow!("no subsurface"));
        };
        let surface = &subsurface.surface;

        let buffer = &mut self.buffers[index];
        buffer.copy_missing(frame.pixels);
        buffer.set_busy();

        surface.attach(Some(&buffer.buffer), 0, 0);
        if !self.shown || surface.version() < wl_surface::REQ_DAMAGE_BUFFER_SINCE {
            surface.damage(0, 0, i32::MAX, i32::MAX);
        } else {
            for rect in damage.rects() {
                surface.damage_buffer(
                    rect.origin.x.0,
                    rect.origin.y.0,
                    rect.size.width.0,
                    rect.size.height.0,
                );
            }
        }
        surface.commit();
        if !self.shown
            && let Some(window_alpha) = &self.window_alpha
        {
            // The subsurface covers the window: nothing of the window
            // surface's last GPU frame is to show through it.
            window_alpha.set_multiplier(0);
        }
        // The subsurface is synchronized: the window surface's commit applies
        // its state, with the frame callback the window requested.
        self.window_surface.commit();
        self.shown = true;

        if let Some(backend) = self.window_surface.backend().upgrade() {
            // A full socket is flushed by the event loop before it sleeps.
            backend.flush().ok();
        }
        Ok(())
    }

    fn gpu_presented(&mut self) {
        self.window_has_buffer = true;
        for buffer in &mut self.buffers {
            buffer.missing.set_all();
        }
        if self.shown {
            if let Some(subsurface) = &self.subsurface {
                // Applied by the window surface's next commit: the GPU frame's.
                subsurface.surface.attach(None, 0, 0);
                subsurface.surface.commit();
            }
            if let Some(window_alpha) = &self.window_alpha {
                window_alpha.set_multiplier(u32::MAX);
            }
            self.shown = false;
        }
    }

    fn release(&mut self) {
        self.buffers.clear();
    }
}

impl Drop for ShmPresenter {
    fn drop(&mut self) {
        WINDOW_SCALES.with_borrow_mut(|scales| scales.remove(&self.window_surface.id()));
        self.buffers.clear();
        if let Some(window_alpha) = self.window_alpha.take() {
            // Destroying it resets the multiplier with the next commit.
            window_alpha.destroy();
        }
        self.subsurface.take();
    }
}

/// Whether the compositor holds a buffer, and whether its presenter dropped
/// it: the release event then destroys it.
#[derive(Default)]
pub(crate) struct BufferState {
    busy: bool,
    dropped: bool,
}

/// A `wl_shm` buffer in its own `memfd`, and the regions it misses relative to
/// the latest frame presented.
struct ShmBuffer {
    buffer: wl_buffer::WlBuffer,
    state: Arc<Mutex<BufferState>>,
    map: Mapping,
    width: u32,
    height: u32,
    format: wl_shm::Format,
    missing: Region,
}

impl ShmBuffer {
    fn new(
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<WaylandClientStatePtr>,
        width: u32,
        height: u32,
        format: wl_shm::Format,
    ) -> anyhow::Result<Self> {
        let stride = width
            .checked_mul(4)
            .filter(|stride| *stride <= i32::MAX as u32)
            .context("CPU frame too wide")?;
        let len = (stride as usize)
            .checked_mul(height as usize)
            .filter(|len| *len <= i32::MAX as usize)
            .context("CPU frame too large for a wl_shm pool")?;
        let map = Mapping::new(len)?;
        let state = Arc::new(Mutex::new(BufferState::default()));
        let pool = shm.create_pool(map.fd().as_fd(), len as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            format,
            qh,
            state.clone(),
        );
        // The buffer keeps the pool's memory.
        pool.destroy();
        Ok(Self {
            buffer,
            state,
            map,
            width,
            height,
            format,
            missing: Region::all(),
        })
    }

    fn matches(&self, width: u32, height: u32, format: wl_shm::Format) -> bool {
        self.width == width && self.height == height && self.format == format
    }

    fn is_busy(&self) -> bool {
        self.state.lock().map(|state| state.busy).unwrap_or(true)
    }

    fn set_busy(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            state.busy = true;
        }
    }

    /// Copies the regions this buffer misses from `pixels`, the latest frame.
    fn copy_missing(&mut self, pixels: &[u32]) {
        let width = self.width as usize;
        let dst = self.map.pixels_mut();
        let all = Bounds {
            origin: Default::default(),
            size: gpui::size(
                DevicePixels(self.width as i32),
                DevicePixels(self.height as i32),
            ),
        };
        let rects = if self.missing.is_all() {
            std::slice::from_ref(&all)
        } else {
            self.missing.rects()
        };
        for rect in rects {
            copy_rect(dst, pixels, width, rect);
        }
        self.missing.clear();
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        // Destroying a buffer the compositor holds would leave the surface's
        // contents undefined: its release destroys it instead.
        let busy = match self.state.lock() {
            Ok(mut state) => {
                state.dropped = true;
                state.busy
            }
            Err(_) => false,
        };
        if !busy {
            self.buffer.destroy();
        }
    }
}

/// Copies `rect` of `src` into `dst`, both frames `width` pixels wide.
fn copy_rect(dst: &mut [u32], src: &[u32], width: usize, rect: &Bounds<DevicePixels>) {
    let x = rect.origin.x.0 as usize;
    let w = rect.size.width.0 as usize;
    for y in rect.origin.y.0 as usize..(rect.origin.y.0 + rect.size.height.0) as usize {
        let start = y * width + x;
        dst[start..start + w].copy_from_slice(&src[start..start + w]);
    }
}

impl Dispatch<wl_buffer::WlBuffer, Arc<Mutex<BufferState>>> for WaylandClientStatePtr {
    fn event(
        _: &mut Self,
        buffer: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        state: &Arc<Mutex<BufferState>>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            let dropped = match state.lock() {
                Ok(mut state) => {
                    state.busy = false;
                    state.dropped
                }
                Err(_) => true,
            };
            if dropped {
                buffer.destroy();
            }
        }
    }
}
