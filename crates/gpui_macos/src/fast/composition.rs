//! Window composition on AppKit: native views between GPUI's base content
//! and its overlays.
//!
//! Ported from zed-industries/zed#62379. The window's own view (`GPUIView`)
//! and its `CAMetalLayer` draw the base GPUI surface. Every other GPUI
//! surface gets a `GPUIOverlayView` of its own, a sibling of the native
//! views, drawn by a renderer that shares the window renderer's device and
//! sprite atlas. `set_composition_order` orders all of them as subviews of
//! the window's view.
//!
//! The default overlay view covers the whole window, but takes mouse events
//! only in GPUI input regions drawn above native content. Custom GPUI
//! surfaces keep their whole-surface input behavior. AppKit descendants keep
//! the result of normal view hit testing on either kind of surface.

use std::{
    any::Any,
    cell::Cell,
    ffi::c_void,
    ptr::{self, NonNull},
    rc::Rc,
    sync::{
        Arc, Weak,
        atomic::{AtomicPtr, Ordering},
    },
};

use anyhow::{Context as _, Result, anyhow};
use cocoa::{
    appkit::{NSView, NSViewHeightSizable, NSViewWidthSizable},
    base::{id, nil},
    foundation::{NSPoint, NSRect, NSSize},
};
use collections::{FxHashMap, FxHashSet};
use gpui::{
    Bounds, ComposedScene, CompositionSurfaceId, DevicePixels, Pixels, PlatformCompositionSurface,
    PlatformCompositionSurfaceContent, PlatformSurfaceAttachment, Point, Size, px,
};
use objc::{
    class,
    declare::ClassDecl,
    msg_send,
    runtime::{Class, NO, Object, Sel, YES},
    sel, sel_impl,
};
use parking_lot::Mutex;

use crate::{
    renderer,
    window::{
        MacWindowState, NSViewLayerContentsRedrawDuringViewResize, WINDOW_STATE_IVAR,
        drop_window_state, get_window_state, handle_view_event,
    },
};

/// The ivar of a `GPUIOverlayView` holding its surface's
/// [`MacGpuiSurface::input`], an `Arc<Mutex<OverlayInput>>` turned into a raw
/// pointer.
const OVERLAY_INPUT_IVAR: &str = "overlayInput";

/// The `GPUIOverlayView` class, registered by [`build_overlay_view_class`].
static OVERLAY_VIEW_CLASS: AtomicPtr<Class> = AtomicPtr::new(ptr::null_mut());

/// A window's composition state: which surface the window's own view draws,
/// and the views and renderers of its other GPUI surfaces.
pub(crate) struct MacComposition {
    /// The GPUI surface drawn by the window's own view and renderer, once the
    /// window composes its scene.
    base_surface: Option<CompositionSurfaceId>,
    /// The default GPUI overlay surface whose input follows deferred hitboxes.
    overlay_surface: Option<CompositionSurfaceId>,
    /// Every GPUI surface but the base one.
    gpui_surfaces: FxHashMap<CompositionSurfaceId, MacGpuiSurface>,
    /// What the window's renderer was created with, for the surfaces'.
    renderer_context: renderer::Context,
    /// Native identities, parents and sibling order, independent of geometry.
    view_order: Vec<(CompositionSurfaceId, usize, usize)>,
}

impl MacComposition {
    pub(crate) fn new(renderer_context: renderer::Context) -> Self {
        Self {
            base_surface: None,
            overlay_surface: None,
            gpui_surfaces: FxHashMap::default(),
            renderer_context,
            view_order: Vec::new(),
        }
    }
}

/// A GPUI surface above the base one: an overlay view and its renderer.
struct MacGpuiSurface {
    view: NonNull<Object>,
    renderer: renderer::Renderer,
    /// What lets this surface receive input; shared with the view.
    input: Arc<Mutex<OverlayInput>>,
}

/// How an overlay view filters the result of AppKit's normal hit testing.
enum OverlayInput {
    /// The default overlay receives input only where GPUI drew a hitbox.
    Regions(Vec<Bounds<Pixels>>),
    /// Custom GPUI surfaces preserve their original whole-surface behavior.
    Full(bool),
}

impl OverlayInput {
    fn accepts(&self, point: Point<Pixels>) -> bool {
        match self {
            Self::Regions(regions) => input_region_contains(regions, point),
            Self::Full(active) => *active,
        }
    }

    fn set_scene_active(&mut self, active: bool) {
        if let Self::Full(current) = self {
            *current = active;
        }
    }

    fn set_regions(&mut self, regions: &[Bounds<Pixels>]) {
        if let Self::Regions(current) = self {
            current.clear();
            current.extend_from_slice(regions);
        }
    }
}

/// Registers the `GPUIOverlayView` class, with the window's other classes.
pub(crate) unsafe fn build_overlay_view_class() {
    let class = if let Some(mut decl) = ClassDecl::new("GPUIOverlayView", class!(NSView)) {
        decl.add_ivar::<*mut c_void>(WINDOW_STATE_IVAR);
        decl.add_ivar::<*mut c_void>(OVERLAY_INPUT_IVAR);
        unsafe {
            decl.add_method(
                sel!(dealloc),
                dealloc_overlay_view as extern "C" fn(&Object, Sel),
            );
            decl.add_method(
                sel!(hitTest:),
                overlay_hit_test as extern "C" fn(&Object, Sel, NSPoint) -> id,
            );
            for selector in [
                sel!(mouseDown:),
                sel!(mouseUp:),
                sel!(rightMouseDown:),
                sel!(rightMouseUp:),
                sel!(otherMouseDown:),
                sel!(otherMouseUp:),
                sel!(mouseMoved:),
                sel!(mouseExited:),
                sel!(mouseDragged:),
                sel!(rightMouseDragged:),
                sel!(otherMouseDragged:),
                sel!(scrollWheel:),
                sel!(magnifyWithEvent:),
                sel!(swipeWithEvent:),
                sel!(pressureChangeWithEvent:),
            ] {
                decl.add_method(
                    selector,
                    handle_overlay_event as extern "C" fn(&Object, Sel, id),
                );
            }
        }
        decl.register()
    } else {
        Class::get("GPUIOverlayView").map_or(ptr::null(), |class| class)
    };
    OVERLAY_VIEW_CLASS.store(class as *mut Class, Ordering::Release);
}

extern "C" fn overlay_hit_test(this: &Object, _: Sel, point: NSPoint) -> id {
    let view = this as *const Object as id;
    let super_hit: id = unsafe { msg_send![super(this, class!(NSView)), hitTest: point] };
    if super_hit != view {
        return filter_overlay_hit(super_hit, view, false);
    }
    let input = unsafe {
        let raw: *mut c_void = *this.get_ivar(OVERLAY_INPUT_IVAR);
        &*(raw as *const Mutex<OverlayInput>)
    };
    let view_bounds = unsafe { NSView::bounds(view) };
    filter_overlay_hit(
        super_hit,
        view,
        input.lock().accepts(appkit_to_gpui(point, view_bounds)),
    )
}

fn filter_overlay_hit(super_hit: id, view: id, accepts_self: bool) -> id {
    if super_hit != view {
        super_hit
    } else if accepts_self {
        view
    } else {
        nil
    }
}

fn appkit_to_gpui(point: NSPoint, view_bounds: NSRect) -> Point<Pixels> {
    Point::new(
        px((point.x - view_bounds.origin.x) as f32),
        px((view_bounds.origin.y + view_bounds.size.height - point.y) as f32),
    )
}

fn input_region_contains(input_region: &[Bounds<Pixels>], point: Point<Pixels>) -> bool {
    input_region.iter().any(|bounds| bounds.contains(&point))
}

extern "C" fn handle_overlay_event(this: &Object, selector: Sel, native_event: id) {
    let window_state = unsafe { get_window_state(this) };
    let native_view = window_state.lock().native_view;
    handle_view_event(unsafe { native_view.as_ref() }, selector, native_event);
}

extern "C" fn dealloc_overlay_view(this: &Object, _: Sel) {
    unsafe {
        drop_window_state(this);
        let raw: *mut c_void = *this.get_ivar(OVERLAY_INPUT_IVAR);
        drop(Arc::from_raw(raw as *const Mutex<OverlayInput>));
        let _: () = msg_send![super(this, class!(NSView)), dealloc];
    }
}

/// `PlatformWindow::draw_composed`: draws each GPUI surface's part of the
/// scene with that surface's renderer, or the whole scene on the window's
/// view until the window composes.
pub(crate) fn draw_composed(window_state: &Arc<Mutex<MacWindowState>>, scene: ComposedScene<'_>) {
    let mut lock = window_state.lock();
    let state = &mut *lock;
    let Some(base_surface) = state.fast_composition.base_surface else {
        state.renderer.draw(scene.scene());
        return;
    };
    for layer in scene.layers() {
        let surface_scene = match scene.layer_scene(layer) {
            Ok(surface_scene) => surface_scene,
            Err(error) => {
                log::error!("replaying AppKit composition scene: {error:#}");
                return;
            }
        };
        if layer.surface == base_surface {
            state.renderer.draw(&surface_scene);
        } else if let Some(surface) = state.fast_composition.gpui_surfaces.get_mut(&layer.surface) {
            surface
                .input
                .lock()
                .set_scene_active(!surface_scene.is_empty());
            surface.renderer.draw(&surface_scene);
        } else {
            log::error!(
                "missing AppKit GPUI composition surface {:?}",
                layer.surface
            );
        }
    }
}

/// Sets the hit regions received by the default GPUI overlay surface.
pub(crate) fn set_input_region(
    window_state: &Arc<Mutex<MacWindowState>>,
    input_region: Option<&[Bounds<Pixels>]>,
) {
    let input_region = input_region.unwrap_or_default();
    let state = window_state.lock();
    if let Some(surface) = state
        .fast_composition
        .overlay_surface
        .and_then(|id| state.fast_composition.gpui_surfaces.get(&id))
    {
        surface.input.lock().set_regions(input_region);
    }
}

/// `PlatformWindow::enable_window_composition`: an AppKit window can always
/// compose, and sets itself up when it's first given an order.
pub(crate) fn enable_window_composition(_window_state: &Arc<Mutex<MacWindowState>>) -> Result<()> {
    Ok(())
}

/// `PlatformWindow::create_native_surface`: a layer-backed `NSView` that
/// clips the native view put in it.
pub(crate) fn create_native_surface(
    window_state: &Arc<Mutex<MacWindowState>>,
) -> Result<Rc<dyn PlatformSurfaceAttachment>> {
    enable_window_composition(window_state)?;
    let state = window_state.lock();
    let parent = state.native_view;

    unsafe {
        let view: id = msg_send![class!(NSView), alloc];
        let view =
            NSView::initWithFrame_(view, NSRect::new(NSPoint::new(0., 0.), NSSize::new(0., 0.)));
        anyhow::ensure!(!view.is_null(), "failed to create native surface container");
        view.setWantsLayer(YES);
        let layer: id = msg_send![view, layer];
        let _: () = msg_send![layer, setMasksToBounds: YES];
        (parent.as_ptr() as id).addSubview_(view);

        Ok(Rc::new(MacNativeSurface {
            window_state: Arc::downgrade(window_state),
            view: NonNull::new(view).context("native surface container is null")?,
            bounds: Cell::new(Bounds::default()),
        }))
    }
}

/// `PlatformWindow::set_composition_order`: creates and removes the views of
/// GPUI surfaces, then puts every surface's view under its parent's, in
/// order, at its bounds.
pub(crate) fn set_composition_order(
    window_state: &Arc<Mutex<MacWindowState>>,
    surfaces: &[PlatformCompositionSurface],
) -> Result<()> {
    let mut lock = window_state.lock();
    let base_surface = surfaces.iter().find_map(|surface| match surface.content {
        PlatformCompositionSurfaceContent::Gpui => Some(surface.id),
        PlatformCompositionSurfaceContent::Native(_)
        | PlatformCompositionSurfaceContent::ExternalGpu(_) => None,
    });
    let base_surface = base_surface.context("composition has no GPUI base surface")?;
    lock.fast_composition.base_surface = Some(base_surface);
    if lock.fast_composition.overlay_surface.is_none() {
        lock.fast_composition.overlay_surface = surfaces.iter().find_map(|surface| {
            matches!(surface.content, PlatformCompositionSurfaceContent::Gpui)
                .then_some(surface.id)
                .filter(|id| *id != base_surface)
        });
    }
    let overlay_surface = lock
        .fast_composition
        .overlay_surface
        .context("composition has no GPUI overlay surface")?;

    let active_gpui_surfaces = surfaces
        .iter()
        .filter_map(|surface| match surface.content {
            PlatformCompositionSurfaceContent::Gpui if surface.id != base_surface => {
                Some(surface.id)
            }
            _ => None,
        })
        .collect::<FxHashSet<_>>();
    let removed_surfaces = lock
        .fast_composition
        .gpui_surfaces
        .keys()
        .filter(|id| !active_gpui_surfaces.contains(id))
        .copied()
        .collect::<Vec<_>>();
    for id in removed_surfaces {
        if let Some(surface) = lock.fast_composition.gpui_surfaces.remove(&id) {
            unsafe { NSView::removeFromSuperview(surface.view.as_ptr()) };
        }
    }

    for surface in surfaces {
        if !matches!(surface.content, PlatformCompositionSurfaceContent::Gpui) {
            continue;
        }
        if surface.id == base_surface
            || lock
                .fast_composition
                .gpui_surfaces
                .contains_key(&surface.id)
        {
            continue;
        }
        let input = if surface.id == overlay_surface {
            OverlayInput::Regions(Vec::new())
        } else {
            OverlayInput::Full(false)
        };
        let created_surface = unsafe { create_gpui_surface(window_state, &mut lock, input)? };
        lock.fast_composition
            .gpui_surfaces
            .insert(surface.id, created_surface);
    }

    let views = surfaces
        .iter()
        .filter_map(|surface| match &surface.content {
            PlatformCompositionSurfaceContent::Gpui => lock
                .fast_composition
                .gpui_surfaces
                .get(&surface.id)
                .map(|gpui_surface| Ok((surface.id, gpui_surface.view.as_ptr() as id))),
            PlatformCompositionSurfaceContent::Native(platform_surface)
            | PlatformCompositionSurfaceContent::ExternalGpu(platform_surface) => {
                Some(platform_surface.platform_handle().and_then(|handle| {
                    handle
                        .downcast::<usize>()
                        .map(|view| (surface.id, *view as id))
                        .map_err(|_| anyhow!("native surface is not backed by an AppKit view"))
                }))
            }
        })
        .collect::<Result<Vec<_>>>()?;

    let views_by_id = views.iter().copied().collect::<FxHashMap<_, _>>();

    unsafe {
        let native_view = lock.native_view.as_ptr() as id;
        let view_order = surfaces
            .iter()
            .filter_map(|surface| {
                let view = views_by_id.get(&surface.id).copied()?;
                let parent = surface
                    .parent
                    .and_then(|parent| views_by_id.get(&parent).copied())
                    .unwrap_or(native_view);
                Some((surface.id, view as usize, parent as usize))
            })
            .collect::<Vec<_>>();
        // A geometry update must preserve AppKit's mouse tracking and first
        // responder. Only a changed tree/order needs views detached and mounted.
        let parents_changed = view_order.iter().any(|(_, view, parent)| {
            let actual_parent: id = msg_send![*view as id, superview];
            actual_parent as usize != *parent
        });
        if lock.fast_composition.view_order != view_order || parents_changed {
            for (_, view, _) in &view_order {
                NSView::removeFromSuperview(*view as id);
            }
            for (_, view, parent) in &view_order {
                (*parent as id).addSubview_(*view as id);
            }
            lock.fast_composition.view_order = view_order;
        }
        for surface in surfaces {
            let Some(view) = views_by_id.get(&surface.id).copied() else {
                continue;
            };
            let parent = surface
                .parent
                .and_then(|parent| views_by_id.get(&parent).copied())
                .unwrap_or(native_view);
            let window_frame = match surface.window_bounds {
                Some(bounds) => window_rect(native_view, bounds, lock.scale_factor()),
                None => NSView::bounds(native_view),
            };
            let frame: NSRect = msg_send![native_view, convertRect: window_frame toView: parent];
            let _: () = msg_send![view, setFrame: frame];
        }
    }
    Ok(())
}

/// Creates the overlay view and renderer of a GPUI surface, the size of the
/// window's view.
unsafe fn create_gpui_surface(
    window_state: &Arc<Mutex<MacWindowState>>,
    state: &mut MacWindowState,
    input: OverlayInput,
) -> Result<MacGpuiSurface> {
    let input = Arc::new(Mutex::new(input));
    let native_view = state.native_view.as_ptr() as id;
    let class = OVERLAY_VIEW_CLASS.load(Ordering::Acquire) as *const Class;
    anyhow::ensure!(!class.is_null(), "GPUIOverlayView is not registered");
    let view: id = unsafe { msg_send![class, alloc] };
    let view = unsafe { NSView::initWithFrame_(view, NSView::bounds(native_view)) };
    anyhow::ensure!(!view.is_null(), "failed to create GPUI composition view");
    unsafe {
        (*view).set_ivar(
            WINDOW_STATE_IVAR,
            Arc::into_raw(window_state.clone()) as *const c_void,
        );
        (*view).set_ivar(
            OVERLAY_INPUT_IVAR,
            Arc::into_raw(input.clone()) as *const c_void,
        );
    }

    let mut renderer = renderer::new_overlay_renderer(
        state.fast_composition.renderer_context.clone(),
        &state.renderer,
    );
    let scale_factor = state.scale_factor();
    renderer.update_drawable_size(state.content_size().to_device_pixels(scale_factor));
    set_contents_scale(&renderer, scale_factor);
    unsafe {
        view.setAutoresizingMask_(NSViewWidthSizable | NSViewHeightSizable);
        view.setWantsLayer(YES);
        let _: () = msg_send![view, setLayer: renderer.layer_ptr()];
        let _: () = msg_send![
            view,
            setLayerContentsRedrawPolicy: NSViewLayerContentsRedrawDuringViewResize
        ];
        let _: id = msg_send![view, autorelease];
    }
    Ok(MacGpuiSurface {
        view: NonNull::new(view).context("GPUI composition view is null")?,
        renderer,
        input,
    })
}

/// Sets whether the window's renderer and every surface's present their
/// drawables in a Core Animation transaction, as they do while AppKit waits
/// for the window to draw.
pub(crate) fn set_presents_with_transaction(state: &mut MacWindowState, enabled: bool) {
    state.renderer.set_presents_with_transaction(enabled);
    for surface in state.fast_composition.gpui_surfaces.values_mut() {
        surface.renderer.set_presents_with_transaction(enabled);
    }
}

/// Follows the window's new scale factor and drawable size on every GPUI
/// surface.
pub(crate) fn scale_factor_changed(
    state: &mut MacWindowState,
    scale_factor: f32,
    drawable_size: Size<DevicePixels>,
) {
    for surface in state.fast_composition.gpui_surfaces.values_mut() {
        set_contents_scale(&surface.renderer, scale_factor);
        surface.renderer.update_drawable_size(drawable_size);
    }
}

/// Follows the window's new drawable size on every GPUI surface.
pub(crate) fn drawable_size_changed(state: &mut MacWindowState, drawable_size: Size<DevicePixels>) {
    for surface in state.fast_composition.gpui_surfaces.values_mut() {
        surface.renderer.update_drawable_size(drawable_size);
    }
}

fn set_contents_scale(renderer: &renderer::Renderer, scale_factor: f32) {
    if let Some(layer) = renderer.layer() {
        unsafe {
            let _: () = msg_send![layer, setContentsScale: scale_factor as f64];
        }
    }
}

/// `bounds`, in window-content device pixels, as a rect in the coordinates of
/// the window's view, whose origin is at its bottom left.
unsafe fn window_rect(native_view: id, bounds: Bounds<DevicePixels>, scale_factor: f32) -> NSRect {
    let bounds = bounds.map(|value| px(value.0 as f32 / scale_factor));
    let window_bounds = unsafe { NSView::bounds(native_view) };
    NSRect::new(
        NSPoint::new(
            f64::from(bounds.origin.x),
            window_bounds.size.height - f64::from(bounds.origin.y) - f64::from(bounds.size.height),
        ),
        NSSize::new(f64::from(bounds.size.width), f64::from(bounds.size.height)),
    )
}

/// A native surface: a layer-backed container view the embedder puts its
/// native view in.
struct MacNativeSurface {
    window_state: Weak<Mutex<MacWindowState>>,
    view: NonNull<Object>,
    bounds: Cell<Bounds<DevicePixels>>,
}

impl PlatformSurfaceAttachment for MacNativeSurface {
    fn set_bounds(&self, bounds: Bounds<DevicePixels>) -> Result<()> {
        let window_state = self
            .window_state
            .upgrade()
            .context("native surface window has been released")?;
        let window_state = window_state.lock();
        unsafe {
            let native_view = window_state.native_view.as_ptr() as id;
            let parent: id = msg_send![self.view.as_ptr(), superview];
            let parent = if parent.is_null() {
                native_view
            } else {
                parent
            };
            let window_frame = window_rect(native_view, bounds, window_state.scale_factor());
            let frame: NSRect = msg_send![native_view, convertRect: window_frame toView: parent];
            let _: () = msg_send![self.view.as_ptr(), setFrame: frame];
        }
        self.bounds.set(bounds);
        Ok(())
    }

    fn bounds(&self) -> Bounds<DevicePixels> {
        self.bounds.get()
    }

    fn set_parent_origin(&self, _origin: Point<DevicePixels>) -> Result<()> {
        self.set_bounds(self.bounds.get())
    }

    fn set_visible(&self, visible: bool) -> Result<()> {
        unsafe {
            let _: () = msg_send![self.view.as_ptr(), setHidden: if visible { NO } else { YES }];
        }
        Ok(())
    }

    fn platform_handle(&self) -> Result<Box<dyn Any>> {
        Ok(Box::new(self.view.as_ptr() as usize))
    }
}

impl Drop for MacNativeSurface {
    fn drop(&mut self) {
        unsafe {
            NSView::removeFromSuperview(self.view.as_ptr());
            let _: () = msg_send![self.view.as_ptr(), release];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{OverlayInput, appkit_to_gpui, filter_overlay_hit, input_region_contains};
    use cocoa::{
        base::nil,
        foundation::{NSPoint, NSRect, NSSize},
    };
    use gpui::{Bounds, Point, px, size};
    use objc::runtime::Object;
    use std::ptr;

    #[test]
    fn converts_appkit_points_to_gpui_coordinates() {
        let view = NSRect::new(NSPoint::new(10., 20.), NSSize::new(300., 200.));

        assert_eq!(
            appkit_to_gpui(NSPoint::new(50., 170.), view),
            Point::new(px(40.), px(50.))
        );
    }

    #[test]
    fn hit_test_uses_input_regions() {
        let input_region = [Bounds::new(
            Point::new(px(20.), px(30.)),
            size(px(80.), px(40.)),
        )];

        assert!(input_region_contains(
            &input_region,
            Point::new(px(20.), px(30.))
        ));
        assert!(input_region_contains(
            &input_region,
            Point::new(px(50.), px(50.))
        ));
        assert!(!input_region_contains(
            &input_region,
            Point::new(px(100.), px(70.))
        ));
        assert!(!input_region_contains(
            &input_region,
            Point::new(px(150.), px(50.))
        ));
    }

    #[test]
    fn hit_test_preserves_descendants() {
        let mut view_value = 0_u8;
        let mut descendant_value = 0_u8;
        let view = ptr::from_mut(&mut view_value).cast::<Object>();
        let descendant = ptr::from_mut(&mut descendant_value).cast::<Object>();

        assert_eq!(filter_overlay_hit(descendant, view, false), descendant);
        assert_eq!(filter_overlay_hit(view, view, false), nil);
        assert_eq!(filter_overlay_hit(view, view, true), view);
    }

    #[test]
    fn custom_surface_keeps_full_input_policy() {
        let point = Point::new(px(150.), px(50.));
        let region = [Bounds::new(
            Point::new(px(20.), px(30.)),
            size(px(80.), px(40.)),
        )];
        let mut custom = OverlayInput::Full(false);
        custom.set_regions(&region);
        assert!(!custom.accepts(point));
        custom.set_scene_active(true);
        assert!(custom.accepts(point));

        let mut default_overlay = OverlayInput::Regions(Vec::new());
        default_overlay.set_scene_active(true);
        assert!(!default_overlay.accepts(point));
        default_overlay.set_regions(&region);
        assert!(default_overlay.accepts(Point::new(px(50.), px(50.))));
        assert!(!default_overlay.accepts(point));
    }
}
