// Modified by Longbridge for gpui-fast.
//! Window composition on Wayland (zed-industries/zed#62379).
//!
//! Every composition surface other than the window's own `wl_surface` is a
//! `wl_subsurface` of it (or of another composition surface): GPUI overlays
//! are synchronized subsurfaces drawn by renderers sharing the window's sprite
//! atlas, native surfaces are desynchronized subsurfaces their producer
//! presents to. Pointer input on a GPUI overlay goes to its window, and an
//! overlay without content has an empty input region so input reaches the
//! surfaces below it.

use std::{
    any::Any,
    cell::{RefCell, RefMut},
    ffi::c_void,
    ptr::NonNull,
    rc::{Rc, Weak},
};

use anyhow::Context as _;
use collections::{FxHashMap, FxHashSet, HashMap};
use raw_window_handle as rwh;
use wayland_backend::client::ObjectId;
use wayland_client::{
    Proxy, delegate_noop,
    protocol::{wl_subcompositor, wl_subsurface, wl_surface},
};
use wayland_protocols::wp::viewporter::client::wp_viewport;

use crate::linux::wayland::window::{RawWindow, WaylandWindow, WaylandWindowStatePtr};
use crate::linux::{Globals, WaylandClientState, WaylandClientStatePtr, get_window};
use gpui::{
    Bounds, ComposedScene, CompositionSurfaceId, DevicePixels, Pixels, PlatformCompositionSurface,
    PlatformCompositionSurfaceContent, PlatformSurfaceAttachment, PlatformWindow, Point, Scene,
    Size,
};
use gpui_wgpu::{WgpuRenderer, WgpuSurfaceConfig, wgpu};

delegate_noop!(WaylandClientStatePtr: ignore wl_subcompositor::WlSubcompositor);
delegate_noop!(WaylandClientStatePtr: ignore wl_subsurface::WlSubsurface);

/// The GPUI composition surfaces that deliver pointer input to their window,
/// by `wl_surface`. Kept by the Wayland client.
#[derive(Default)]
pub(crate) struct InputSurfaces(HashMap<ObjectId, WaylandWindowStatePtr>);

/// Resolves the window receiving pointer input on `surface_id`, including GPUI
/// composition subsurfaces, which share their window's coordinate space.
pub(crate) fn get_input_window(
    state: &mut RefMut<WaylandClientState>,
    surface_id: &ObjectId,
) -> Option<WaylandWindowStatePtr> {
    get_window(state, surface_id).or_else(|| {
        state
            .fast_composition_input_surfaces
            .0
            .get(surface_id)
            .cloned()
    })
}

/// Forgets the input surfaces of a window that closed.
pub(crate) fn forget_input_surfaces(
    state: &mut WaylandClientState,
    closed_window: &WaylandWindowStatePtr,
) {
    state
        .fast_composition_input_surfaces
        .0
        .retain(|_, window| !window.ptr_eq(closed_window));
}

impl WaylandClientStatePtr {
    fn register_composition_input_surface(
        &self,
        surface_id: ObjectId,
        window: WaylandWindowStatePtr,
    ) {
        self.get_client()
            .borrow_mut()
            .fast_composition_input_surfaces
            .0
            .insert(surface_id, window);
    }

    fn unregister_composition_input_surface(&self, surface_id: &ObjectId) {
        self.get_client()
            .borrow_mut()
            .fast_composition_input_surfaces
            .0
            .remove(surface_id);
    }
}

/// A `wl_surface` stacked inside a window through `wl_subsurface`.
///
/// A `wl_subsurface`'s parent is fixed when it is created, so reparenting
/// destroys the role object and requests a new one for the same surface.
/// Destroying the role object also unmaps the surface, which is how hidden
/// surfaces leave the composition.
struct WaylandSubsurface {
    surface: wl_surface::WlSurface,
    subsurface: Option<wl_subsurface::WlSubsurface>,
    parent: Option<ObjectId>,
    viewport: Option<wp_viewport::WpViewport>,
    synchronized: bool,
}

impl WaylandSubsurface {
    fn new(globals: &Globals, synchronized: bool) -> anyhow::Result<Self> {
        anyhow::ensure!(
            globals.fast_subcompositor.is_some(),
            "the Wayland compositor does not support wl_subcompositor"
        );
        let surface = globals.compositor.create_surface(&globals.qh, ());
        let viewport = globals
            .viewporter
            .as_ref()
            .map(|viewporter| viewporter.get_viewport(&surface, &globals.qh, ()));
        Ok(Self {
            surface,
            subsurface: None,
            parent: None,
            viewport,
            synchronized,
        })
    }

    fn attach(
        &mut self,
        globals: &Globals,
        parent: &wl_surface::WlSurface,
    ) -> anyhow::Result<&wl_subsurface::WlSubsurface> {
        if self.parent.as_ref() != Some(&parent.id()) {
            self.detach();
        }
        if self.subsurface.is_none() {
            let subcompositor = globals
                .fast_subcompositor
                .as_ref()
                .context("the Wayland compositor does not support wl_subcompositor")?;
            let subsurface = subcompositor.get_subsurface(&self.surface, parent, &globals.qh, ());
            if self.synchronized {
                subsurface.set_sync();
            } else {
                subsurface.set_desync();
            }
            self.subsurface = Some(subsurface);
            self.parent = Some(parent.id());
        }
        self.subsurface
            .as_ref()
            .context("Wayland subsurface was not created")
    }

    fn detach(&mut self) {
        if let Some(subsurface) = self.subsurface.take() {
            subsurface.destroy();
        }
        self.parent = None;
    }

    fn set_size(&self, size: Size<DevicePixels>, scale: f32) {
        if let Some(viewport) = &self.viewport {
            let size = logical_size(size, scale);
            viewport.set_destination(size.width.max(1), size.height.max(1));
        } else {
            self.surface.set_buffer_scale(scale.ceil().max(1.) as i32);
        }
    }

    fn destroy(&mut self) {
        self.detach();
        // The viewport must be destroyed before its wl_surface.
        if let Some(viewport) = self.viewport.take() {
            viewport.destroy();
        }
        self.surface.destroy();
    }
}

fn logical_size(size: Size<DevicePixels>, scale: f32) -> Size<i32> {
    size.map(|value| (value.0 as f32 / scale).round() as i32)
}

fn logical_point(point: Point<DevicePixels>, scale: f32) -> Point<i32> {
    point.map(|value| (value.0 as f32 / scale).round() as i32)
}

fn raw_window(surface: &wl_surface::WlSurface) -> anyhow::Result<RawWindow> {
    Ok(RawWindow {
        window: surface.id().as_ptr().cast::<c_void>(),
        display: surface
            .backend()
            .upgrade()
            .context("Wayland connection closed")?
            .display_ptr()
            .cast::<c_void>(),
    })
}

struct WaylandGpuiSurface {
    subsurface: WaylandSubsurface,
    renderer: WgpuRenderer,
    input_active: Option<bool>,
}

impl WaylandGpuiSurface {
    fn raw_window(&self) -> anyhow::Result<RawWindow> {
        raw_window(&self.subsurface.surface)
    }

    fn surface_config(size: Size<DevicePixels>) -> WgpuSurfaceConfig {
        WgpuSurfaceConfig {
            size,
            transparent: true,
            preferred_present_mode: Some(wgpu::PresentMode::Mailbox),
        }
    }

    fn destroy(&mut self, client: &WaylandClientStatePtr) {
        client.unregister_composition_input_surface(&self.subsurface.surface.id());
        // Release the wgpu surface before the wl_surface it presents to.
        self.renderer.destroy();
        self.subsurface.destroy();
    }
}

struct WaylandNativeSurfaceState {
    subsurface: WaylandSubsurface,
    bounds: Bounds<DevicePixels>,
    parent_origin: Point<DevicePixels>,
    visible: bool,
}

impl WaylandNativeSurfaceState {
    fn update_geometry(&self, scale: f32) {
        if let Some(subsurface) = &self.subsurface.subsurface {
            let position = logical_point(self.bounds.origin - self.parent_origin, scale);
            subsurface.set_position(position.x, position.y);
        }
        self.subsurface.set_size(self.bounds.size, scale);
    }
}

/// A `wl_surface` slot for content produced outside GPUI's renderer. Its
/// `platform_handle` is a `raw_window_handle::RawWindowHandle::Wayland`, which
/// the producer must stop presenting to before this attachment is dropped.
struct WaylandNativeSurface {
    state: Rc<RefCell<WaylandNativeSurfaceState>>,
    composition: Weak<RefCell<WaylandComposition>>,
}

impl WaylandNativeSurface {
    fn composition(&self) -> anyhow::Result<Rc<RefCell<WaylandComposition>>> {
        self.composition
            .upgrade()
            .context("native surface window has been released")
    }
}

impl PlatformSurfaceAttachment for WaylandNativeSurface {
    fn set_bounds(&self, bounds: Bounds<DevicePixels>) -> anyhow::Result<()> {
        let scale = self.composition()?.borrow().scale;
        let mut state = self.state.borrow_mut();
        state.bounds = bounds;
        state.update_geometry(scale);
        Ok(())
    }

    fn bounds(&self) -> Bounds<DevicePixels> {
        self.state.borrow().bounds
    }

    fn set_parent_origin(&self, origin: Point<DevicePixels>) -> anyhow::Result<()> {
        let scale = self.composition()?.borrow().scale;
        let mut state = self.state.borrow_mut();
        state.parent_origin = origin;
        state.update_geometry(scale);
        Ok(())
    }

    fn set_visible(&self, visible: bool) -> anyhow::Result<()> {
        let composition = self.composition()?;
        let mut state = self.state.borrow_mut();
        if state.visible == visible {
            return Ok(());
        }
        state.visible = visible;
        if visible {
            drop(state);
            // Remapping needs the surface's parent and siblings, which only the
            // last applied composition order knows.
            composition.borrow_mut().apply_order()
        } else {
            state.subsurface.detach();
            Ok(())
        }
    }

    fn platform_handle(&self) -> anyhow::Result<Box<dyn Any>> {
        let surface = self.state.borrow().subsurface.surface.id().as_ptr();
        let surface = NonNull::new(surface.cast::<c_void>()).context("native surface is null")?;
        Ok(Box::new(rwh::RawWindowHandle::Wayland(
            rwh::WaylandWindowHandle::new(surface),
        )))
    }
}

impl Drop for WaylandNativeSurface {
    fn drop(&mut self) {
        self.state.borrow_mut().subsurface.destroy();
    }
}

/// A Wayland window's composition state, held by `WaylandWindowState`.
pub(crate) struct Composition(Rc<RefCell<WaylandComposition>>);

impl Composition {
    pub(crate) fn new(
        globals: &Globals,
        client: &WaylandClientStatePtr,
        window_surface: &wl_surface::WlSurface,
        size: Size<Pixels>,
    ) -> Self {
        Self(Rc::new(RefCell::new(WaylandComposition {
            globals: globals.clone(),
            client: client.clone(),
            window_surface: window_surface.clone(),
            size: size.map(|value| DevicePixels(f32::from(value) as i32)),
            scale: 1.0,
            base_surface: None,
            gpui_surfaces: FxHashMap::default(),
            native_surfaces: Vec::new(),
            order: Vec::new(),
            renderers_need_recreation: false,
        })))
    }

    /// Releases every composition surface; called when the window is dropped,
    /// before its renderer and `wl_surface` are destroyed.
    pub(crate) fn destroy(&self) {
        self.0.borrow_mut().destroy();
    }

    /// Resizes the GPUI overlays with the window.
    pub(crate) fn resize(&self, size: Size<DevicePixels>, scale: f32) {
        self.0.borrow_mut().resize(size, scale);
    }

    /// Notes that the window's renderer recovered from a lost device: the
    /// overlays' renderers belong to the lost device and are created again
    /// from the new one before the next composed frame.
    pub(crate) fn renderers_lost(&self) {
        self.0.borrow_mut().renderers_need_recreation = true;
    }
}

/// The subsurfaces of a window that uses window composition. GPUI surfaces
/// other than the base are synchronized with the window surface, so their
/// frames appear atomically with the base frame's commit.
struct WaylandComposition {
    globals: Globals,
    client: WaylandClientStatePtr,
    window_surface: wl_surface::WlSurface,
    size: Size<DevicePixels>,
    scale: f32,
    base_surface: Option<CompositionSurfaceId>,
    gpui_surfaces: FxHashMap<CompositionSurfaceId, WaylandGpuiSurface>,
    native_surfaces: Vec<Weak<RefCell<WaylandNativeSurfaceState>>>,
    order: Vec<PlatformCompositionSurface>,
    renderers_need_recreation: bool,
}

impl WaylandComposition {
    fn native_surface(
        &mut self,
        handle: &dyn PlatformSurfaceAttachment,
    ) -> anyhow::Result<Rc<RefCell<WaylandNativeSurfaceState>>> {
        let handle = handle
            .platform_handle()?
            .downcast::<rwh::RawWindowHandle>()
            .map_err(|_| anyhow::anyhow!("native surface is not backed by a Wayland surface"))?;
        let rwh::RawWindowHandle::Wayland(handle) = *handle else {
            anyhow::bail!("native surface is not backed by a Wayland surface");
        };
        self.native_surfaces
            .retain(|surface| surface.strong_count() > 0);
        self.native_surfaces
            .iter()
            .filter_map(Weak::upgrade)
            .find(|surface| {
                surface
                    .borrow()
                    .subsurface
                    .surface
                    .id()
                    .as_ptr()
                    .cast::<c_void>()
                    == handle.surface.as_ptr()
            })
            .context("native surface belongs to another window")
    }

    fn apply_order(&mut self) -> anyhow::Result<()> {
        let base = self
            .base_surface
            .context("composition has no GPUI base surface")?;
        let order = self.order.clone();

        let mut native_states = FxHashMap::default();
        for surface in &order {
            if let PlatformCompositionSurfaceContent::Native(attachment)
            | PlatformCompositionSurfaceContent::ExternalGpu(attachment) = &surface.content
            {
                native_states.insert(surface.id, self.native_surface(attachment.as_ref())?);
            }
        }

        let mut wl_surfaces = FxHashMap::default();
        wl_surfaces.insert(base, self.window_surface.clone());
        for (id, surface) in &self.gpui_surfaces {
            wl_surfaces.insert(*id, surface.subsurface.surface.clone());
        }
        for (id, state) in &native_states {
            wl_surfaces.insert(*id, state.borrow().subsurface.surface.clone());
        }
        let mut origins = FxHashMap::default();
        origins.insert(base, Point::default());
        for surface in &order {
            origins.insert(surface.id, surface.window_origin);
        }

        // Sibling order follows the flattened tree, bottom to top. Root
        // surfaces listed before the base are stacked below the window surface
        // and the others above it; a surface whose parent is the base is a
        // child of the window surface as well.
        let base_index = order
            .iter()
            .position(|surface| surface.id == base)
            .context("composition order is missing its base surface")?;
        let mut above = Vec::new();
        let mut below = Vec::new();
        for (index, surface) in order.iter().enumerate() {
            if surface.id == base {
                continue;
            }
            match surface.parent {
                None if index < base_index => below.push(surface),
                None => above.push((surface, base)),
                Some(parent) => above.push((surface, parent)),
            }
        }

        let mut last_below: Option<wl_surface::WlSurface> = None;
        for surface in below.into_iter().rev() {
            let window_surface = self.window_surface.clone();
            let Some(subsurface) = self.attach(surface.id, &native_states, &window_surface)? else {
                continue;
            };
            subsurface.place_below(last_below.as_ref().unwrap_or(&window_surface));
            let position = logical_point(surface.window_origin, self.scale);
            subsurface.set_position(position.x, position.y);
            last_below = wl_surfaces.get(&surface.id).cloned();
        }

        let mut last_above: FxHashMap<CompositionSurfaceId, wl_surface::WlSurface> =
            FxHashMap::default();
        for (surface, parent) in above {
            let parent_surface = wl_surfaces
                .get(&parent)
                .cloned()
                .context("composition parent surface is missing")?;
            let Some(subsurface) = self.attach(surface.id, &native_states, &parent_surface)? else {
                continue;
            };
            subsurface.place_above(last_above.get(&parent).unwrap_or(&parent_surface));
            let parent_origin = origins.get(&parent).copied().unwrap_or_default();
            let position = logical_point(surface.window_origin - parent_origin, self.scale);
            subsurface.set_position(position.x, position.y);
            if let Some(wl_surface) = wl_surfaces.get(&surface.id) {
                last_above.insert(parent, wl_surface.clone());
            }
        }

        for surface in &order {
            if let Some(state) = native_states.get(&surface.id) {
                let mut state = state.borrow_mut();
                state.parent_origin = origins
                    .get(&surface.parent.unwrap_or(base))
                    .copied()
                    .unwrap_or_default();
                if let Some(bounds) = surface.window_bounds {
                    state.bounds = bounds;
                }
                state.update_geometry(self.scale);
            }
        }
        Ok(())
    }

    /// Attaches a surface under `parent`, or returns `None` for a hidden
    /// native surface, which stays unmapped until it is shown again.
    fn attach(
        &mut self,
        id: CompositionSurfaceId,
        native_states: &FxHashMap<CompositionSurfaceId, Rc<RefCell<WaylandNativeSurfaceState>>>,
        parent: &wl_surface::WlSurface,
    ) -> anyhow::Result<Option<wl_subsurface::WlSubsurface>> {
        if let Some(surface) = self.gpui_surfaces.get_mut(&id) {
            return Ok(Some(
                surface.subsurface.attach(&self.globals, parent)?.clone(),
            ));
        }
        if let Some(state) = native_states.get(&id) {
            let mut state = state.borrow_mut();
            if !state.visible {
                return Ok(None);
            }
            return Ok(Some(
                state.subsurface.attach(&self.globals, parent)?.clone(),
            ));
        }
        Ok(None)
    }

    fn resize(&mut self, size: Size<DevicePixels>, scale: f32) {
        self.size = size;
        self.scale = scale;
        for surface in self.gpui_surfaces.values_mut() {
            surface.renderer.update_drawable_size(size);
            surface.subsurface.set_size(size, scale);
        }
        self.native_surfaces
            .retain(|surface| surface.strong_count() > 0);
        for surface in self.native_surfaces.iter().filter_map(Weak::upgrade) {
            surface.borrow().update_geometry(scale);
        }
    }

    fn destroy(&mut self) {
        for (_, mut surface) in self.gpui_surfaces.drain() {
            surface.destroy(&self.client);
        }
        for surface in self
            .native_surfaces
            .drain(..)
            .filter_map(|surface| surface.upgrade())
        {
            surface.borrow_mut().subsurface.detach();
        }
        self.order.clear();
        self.base_surface = None;
    }
}

/// `PlatformWindow::draw_composed`: draws each GPUI overlay on its
/// subsurface, then the base content on the window surface, whose commit
/// makes the synchronized overlays' frames appear with it.
pub(crate) fn draw_composed(window: &WaylandWindow, scene: ComposedScene<'_>) {
    let state = window.borrow();
    let composition = state.fast_composition.0.clone();
    let mut composition_ref = composition.borrow_mut();
    let composition = &mut *composition_ref;
    let Some(base_surface) = composition.base_surface else {
        drop(composition_ref);
        drop(state);
        window.draw(scene.scene());
        return;
    };

    if composition.renderers_need_recreation
        && !state
            .renderer
            .as_ref()
            .expect("the window has a renderer")
            .device_lost()
    {
        composition.renderers_need_recreation = false;
        let size = composition.size;
        let mut failed = false;
        for surface in composition.gpui_surfaces.values_mut() {
            let renderer = surface.raw_window().and_then(|raw_window| {
                state
                    .renderer
                    .as_ref()
                    .expect("the window has a renderer")
                    .new_sharing_atlas(&raw_window, WaylandGpuiSurface::surface_config(size))
            });
            match renderer {
                Ok(renderer) => surface.renderer = renderer,
                Err(error) => {
                    log::error!("recreating Wayland composition renderer: {error:#}");
                    failed = true;
                }
            }
        }
        composition.renderers_need_recreation = failed;
    }
    drop(state);

    let mut base_scene = Scene::default();
    for layer in scene.layers() {
        let surface_scene = match scene.layer_scene(layer) {
            Ok(surface_scene) => surface_scene,
            Err(error) => {
                log::error!("replaying Wayland composition scene: {error:#}");
                return;
            }
        };
        if layer.surface == base_surface {
            base_scene = surface_scene;
        } else if let Some(surface) = composition.gpui_surfaces.get_mut(&layer.surface) {
            // An overlay without content must let input reach the surfaces
            // below it, like the AppKit overlay's hit testing.
            let input_active = !surface_scene.is_empty();
            if surface.input_active != Some(input_active) {
                if input_active {
                    surface.subsurface.surface.set_input_region(None);
                } else {
                    let region = composition
                        .globals
                        .compositor
                        .create_region(&composition.globals.qh, ());
                    surface.subsurface.surface.set_input_region(Some(&region));
                    region.destroy();
                }
                surface.input_active = Some(input_active);
            }
            // Synchronized subsurfaces cache this commit until the window
            // surface commits the base frame below.
            surface.renderer.draw(&surface_scene);
        } else {
            log::error!(
                "missing Wayland GPUI composition surface {:?}",
                layer.surface
            );
        }
    }
    drop(composition_ref);
    window.draw(&base_scene);
}

/// `PlatformWindow::enable_window_composition`: composition needs
/// `wl_subcompositor`.
pub(crate) fn enable_window_composition(window: &WaylandWindow) -> anyhow::Result<()> {
    anyhow::ensure!(
        window.borrow().globals.fast_subcompositor.is_some(),
        "the Wayland compositor does not support wl_subcompositor"
    );
    Ok(())
}

/// `PlatformWindow::create_native_surface`: a desynchronized subsurface,
/// mapped once the composition order places it.
pub(crate) fn create_native_surface(
    window: &WaylandWindow,
) -> anyhow::Result<Rc<dyn PlatformSurfaceAttachment>> {
    enable_window_composition(window)?;
    let state = window.borrow();
    let subsurface = WaylandSubsurface::new(&state.globals, false)?;
    let native_state = Rc::new(RefCell::new(WaylandNativeSurfaceState {
        subsurface,
        bounds: Bounds::default(),
        parent_origin: Point::default(),
        visible: true,
    }));
    state
        .fast_composition
        .0
        .borrow_mut()
        .native_surfaces
        .push(Rc::downgrade(&native_state));
    Ok(Rc::new(WaylandNativeSurface {
        state: native_state,
        composition: Rc::downgrade(&state.fast_composition.0),
    }))
}

/// `PlatformWindow::set_composition_order`: creates and destroys the GPUI
/// overlays' subsurfaces and restacks every subsurface in `surfaces`' order.
pub(crate) fn set_composition_order(
    window: &WaylandWindow,
    surfaces: &[PlatformCompositionSurface],
) -> anyhow::Result<()> {
    let state = window.borrow();
    let base_surface = surfaces
        .iter()
        .find_map(|surface| match surface.content {
            PlatformCompositionSurfaceContent::Gpui => Some(surface.id),
            PlatformCompositionSurfaceContent::Native(_)
            | PlatformCompositionSurfaceContent::ExternalGpu(_) => None,
        })
        .context("composition has no GPUI base surface")?;
    let mut composition = state.fast_composition.0.borrow_mut();

    let active_gpui_surfaces = surfaces
        .iter()
        .filter(|surface| {
            surface.id != base_surface
                && matches!(surface.content, PlatformCompositionSurfaceContent::Gpui)
        })
        .map(|surface| surface.id)
        .collect::<FxHashSet<_>>();
    let removed_surfaces = composition
        .gpui_surfaces
        .keys()
        .filter(|id| !active_gpui_surfaces.contains(id))
        .copied()
        .collect::<Vec<_>>();
    for id in removed_surfaces {
        if let Some(mut surface) = composition.gpui_surfaces.remove(&id) {
            surface.destroy(&state.client);
        }
    }

    for id in active_gpui_surfaces {
        if composition.gpui_surfaces.contains_key(&id) {
            continue;
        }
        let mut subsurface = WaylandSubsurface::new(&state.globals, true)?;
        subsurface.set_size(composition.size, composition.scale);
        let renderer = raw_window(&subsurface.surface).and_then(|raw_window| {
            state
                .renderer
                .as_ref()
                .expect("the window has a renderer")
                .new_sharing_atlas(
                    &raw_window,
                    WaylandGpuiSurface::surface_config(composition.size),
                )
        });
        let renderer = match renderer {
            Ok(renderer) => renderer,
            Err(error) => {
                subsurface.destroy();
                return Err(error);
            }
        };
        let surface = WaylandGpuiSurface {
            subsurface,
            renderer,
            input_active: None,
        };
        state
            .client
            .register_composition_input_surface(surface.subsurface.surface.id(), window.0.clone());
        composition.gpui_surfaces.insert(id, surface);
    }

    composition.base_surface = Some(base_surface);
    composition.order = surfaces.to_vec();
    composition.apply_order()
}
