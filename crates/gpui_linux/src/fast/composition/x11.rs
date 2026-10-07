//! Window composition on X11 (zed-industries/zed#62379).
//!
//! X11 draws every GPUI composition surface into the window itself: a child
//! window with the same ARGB visual as its parent replaces the parent's pixels
//! instead of blending over them, so GPUI overlays cannot be child windows.
//! Native surfaces are child windows, and the GPUI content stacked above one
//! is cut out of its shape (the SHAPE extension) so the window's pixels and
//! input show through.
//!
//! A cutout can only replace native pixels, so shadows and translucent
//! content (a dialog's backdrop) cannot cover a native surface that way. When
//! a compositing manager blends windows with an ARGB visual, the GPUI content
//! stacked above the native surfaces is instead drawn into a transparent
//! override-redirect window kept over the window, which the compositing
//! manager blends over the native content. Its input shape is its content, so
//! input elsewhere reaches the windows below it, and its input is delivered
//! to the window it covers.

use std::{
    any::Any,
    cell::RefCell,
    num::NonZeroU32,
    rc::{Rc, Weak},
};

use anyhow::{Context as _, anyhow};
use collections::{FxHashMap, FxHashSet};
use gpui::{
    Bounds, ComposedScene, CompositionSurfaceId, ContentMask, DevicePixels,
    PlatformCompositionSurface, PlatformCompositionSurfaceContent, PlatformSurfaceAttachment,
    PlatformWindow, Point, ScaledPixels, Scene, Size,
};
use gpui_util::ResultExt as _;
use gpui_wgpu::{WgpuRenderer, WgpuSurfaceConfig, wgpu};
use raw_window_handle as rwh;
use x11rb::{
    connection::{Connection as _, RequestConnection as _},
    protocol::{
        shape::{self, ConnectionExt as _},
        xinput::{self, ConnectionExt as _},
        xproto::{self, ConnectionExt as _},
    },
    xcb_ffi::XCBConnection,
};

use crate::linux::{
    X11Window, X11WindowStatePtr, XINPUT_ALL_DEVICE_GROUPS, check_reply, get_reply, xcb_flush,
};

/// An X11 window's composition state, held by `X11WindowState`.
pub(crate) struct Composition(Rc<RefCell<X11Composition>>);

impl Composition {
    pub(crate) fn new(
        xcb: &Rc<XCBConnection>,
        x_window: xproto::Window,
        depth: u8,
        visual_id: u32,
    ) -> Self {
        Self(Rc::new(RefCell::new(X11Composition {
            xcb: xcb.clone(),
            x_window,
            depth,
            visual_id,
            base_surface: None,
            native_surfaces: Vec::new(),
            order: Vec::new(),
            occluders: FxHashMap::default(),
            overlay: Overlay::Unknown,
        })))
    }

    /// Destroys the native surfaces' child windows; called when the window
    /// is dropped, before its renderer and X window are destroyed.
    pub(crate) fn destroy(&self) {
        self.0.borrow_mut().destroy();
    }
}

struct X11Composition {
    xcb: Rc<XCBConnection>,
    x_window: xproto::Window,
    depth: u8,
    visual_id: u32,
    base_surface: Option<CompositionSurfaceId>,
    native_surfaces: Vec<Weak<RefCell<X11NativeSurfaceState>>>,
    order: Vec<PlatformCompositionSurface>,
    occluders: FxHashMap<CompositionSurfaceId, Vec<Bounds<DevicePixels>>>,
    overlay: Overlay,
}

struct X11NativeSurfaceState {
    xcb: Rc<XCBConnection>,
    x_window: xproto::Window,
    parent_window: xproto::Window,
    bounds: Bounds<DevicePixels>,
    parent_origin: Point<DevicePixels>,
    visible: bool,
    mapped: bool,
    /// Window-coordinate rectangles of GPUI content stacked above this surface.
    occluders: Vec<Bounds<DevicePixels>>,
    applied_shape: Option<(Bounds<DevicePixels>, Vec<Bounds<DevicePixels>>)>,
    destroyed: bool,
}

impl X11NativeSurfaceState {
    fn apply(&mut self) -> anyhow::Result<()> {
        if self.destroyed {
            return Ok(());
        }
        let origin = self.bounds.origin - self.parent_origin;
        check_reply(
            || "X11 ConfigureWindow for a native surface failed.",
            self.xcb.configure_window(
                self.x_window,
                &xproto::ConfigureWindowAux::new()
                    .x(origin.x.0)
                    .y(origin.y.0)
                    .width(self.bounds.size.width.0.max(1) as u32)
                    .height(self.bounds.size.height.0.max(1) as u32),
            ),
        )?;
        self.apply_shape()?;
        let should_map = self.visible && !self.bounds.is_empty();
        if should_map != self.mapped {
            if should_map {
                check_reply(
                    || "X11 MapWindow for a native surface failed.",
                    self.xcb.map_window(self.x_window),
                )?;
            } else {
                check_reply(
                    || "X11 UnmapWindow for a native surface failed.",
                    self.xcb.unmap_window(self.x_window),
                )?;
            }
            self.mapped = should_map;
        }
        xcb_flush(&self.xcb);
        Ok(())
    }

    fn apply_shape(&mut self) -> anyhow::Result<()> {
        let local_bounds = Bounds::new(Point::default(), self.bounds.size);
        let cutouts = self
            .occluders
            .iter()
            .map(|occluder| {
                let occluder = Bounds::new(occluder.origin - self.bounds.origin, occluder.size);
                occluder.intersect(&local_bounds)
            })
            .filter(|occluder| !occluder.is_empty())
            .collect::<Vec<_>>();
        let shape = (self.bounds, cutouts);
        if self.applied_shape.as_ref() == Some(&shape) {
            return Ok(());
        }
        let (_, cutouts) = &shape;
        if cutouts.is_empty() {
            check_reply(
                || "X11 ShapeMask reset for a native surface failed.",
                self.xcb.shape_mask(
                    shape::SO::SET,
                    shape::SK::BOUNDING,
                    self.x_window,
                    0,
                    0,
                    x11rb::NONE,
                ),
            )?;
        } else {
            check_reply(
                || "X11 ShapeRectangles for a native surface failed.",
                self.xcb.shape_rectangles(
                    shape::SO::SET,
                    shape::SK::BOUNDING,
                    xproto::ClipOrdering::UNSORTED,
                    self.x_window,
                    0,
                    0,
                    &[x11_rectangle(local_bounds)],
                ),
            )?;
            check_reply(
                || "X11 ShapeRectangles cutout for a native surface failed.",
                self.xcb.shape_rectangles(
                    shape::SO::SUBTRACT,
                    shape::SK::BOUNDING,
                    xproto::ClipOrdering::UNSORTED,
                    self.x_window,
                    0,
                    0,
                    &cutouts
                        .iter()
                        .copied()
                        .map(x11_rectangle)
                        .collect::<Vec<_>>(),
                ),
            )?;
        }
        self.applied_shape = Some(shape);
        Ok(())
    }

    fn destroy(&mut self) {
        if std::mem::replace(&mut self.destroyed, true) {
            return;
        }
        check_reply(
            || "X11 DestroyWindow for a native surface failed.",
            self.xcb.destroy_window(self.x_window),
        )
        .log_err();
        xcb_flush(&self.xcb);
    }
}

fn x11_rectangle(bounds: Bounds<DevicePixels>) -> xproto::Rectangle {
    xproto::Rectangle {
        x: bounds.origin.x.0.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
        y: bounds.origin.y.0.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
        width: bounds.size.width.0.clamp(0, u16::MAX as i32) as u16,
        height: bounds.size.height.0.clamp(0, u16::MAX as i32) as u16,
    }
}

/// Past this many rectangles an occluding surface is cut out by its bounding
/// box, which keeps shape requests small for text-heavy overlays.
const MAX_OCCLUDER_RECTANGLES: usize = 256;

/// Returns the device-pixel rectangles covered by a scene's primitives.
///
/// Shadows are left out: a cutout can only show the window's pixels, not blend
/// a translucent shadow over the native content, so cutting out a shadow would
/// replace native content with GPUI's background around every overlay.
fn scene_occluders(scene: &Scene, opaque_only: bool) -> Vec<Bounds<DevicePixels>> {
    fn clipped(
        bounds: Bounds<ScaledPixels>,
        content_mask: &ContentMask<ScaledPixels>,
    ) -> Bounds<ScaledPixels> {
        bounds.intersect(&content_mask.bounds)
    }

    let mut rectangles = Vec::new();
    rectangles.extend(
        scene
            .quads
            .iter()
            .filter(|quad| !opaque_only || quad.background.is_opaque())
            .map(|quad| clipped(quad.bounds, &quad.content_mask)),
    );
    rectangles.extend(
        scene
            .paths
            .iter()
            .map(|path| clipped(path.bounds, &path.content_mask)),
    );
    rectangles.extend(
        scene
            .underlines
            .iter()
            .map(|underline| clipped(underline.bounds, &underline.content_mask)),
    );
    rectangles.extend(
        scene
            .monochrome_sprites
            .iter()
            .map(|sprite| clipped(sprite.bounds, &sprite.content_mask)),
    );
    rectangles.extend(
        scene
            .subpixel_sprites
            .iter()
            .map(|sprite| clipped(sprite.bounds, &sprite.content_mask)),
    );
    rectangles.extend(
        scene
            .polychrome_sprites
            .iter()
            .map(|sprite| clipped(sprite.bounds, &sprite.content_mask)),
    );
    rectangles.extend(
        scene
            .surfaces
            .iter()
            .map(|surface| clipped(surface.bounds, &surface.content_mask)),
    );

    let mut rectangles = rectangles
        .into_iter()
        .map(|bounds| {
            let origin = bounds
                .origin
                .map(|value| DevicePixels(value.0.floor() as i32));
            let corner = bounds
                .bottom_right()
                .map(|value| DevicePixels(value.0.ceil() as i32));
            Bounds::from_corners(origin, corner)
        })
        .filter(|bounds| !bounds.is_empty())
        .collect::<Vec<_>>();

    // Content usually sits on a background quad, so most rectangles are
    // covered by a larger one.
    rectangles.sort_by_key(|bounds| std::cmp::Reverse(bounds.size.width.0 * bounds.size.height.0));
    let mut kept: Vec<Bounds<DevicePixels>> = Vec::new();
    for rectangle in rectangles {
        let covered = kept.iter().any(|kept| {
            kept.origin.x <= rectangle.origin.x
                && kept.origin.y <= rectangle.origin.y
                && kept.bottom_right().x >= rectangle.bottom_right().x
                && kept.bottom_right().y >= rectangle.bottom_right().y
        });
        if !covered {
            kept.push(rectangle);
        }
    }
    if kept.len() > MAX_OCCLUDER_RECTANGLES {
        let union = kept
            .iter()
            .copied()
            .reduce(|union, bounds| union.union(&bounds))
            .into_iter()
            .collect();
        return union;
    }
    kept
}

/// A child window slot for content produced outside GPUI's renderer. Its
/// `platform_handle` is a `raw_window_handle::RawWindowHandle::Xcb`, which the
/// producer must stop presenting to before this attachment is dropped.
struct X11NativeSurface {
    state: Rc<RefCell<X11NativeSurfaceState>>,
    visual_id: u32,
}

impl PlatformSurfaceAttachment for X11NativeSurface {
    fn set_bounds(&self, bounds: Bounds<DevicePixels>) -> anyhow::Result<()> {
        let mut state = self.state.borrow_mut();
        state.bounds = bounds;
        state.apply()
    }

    fn bounds(&self) -> Bounds<DevicePixels> {
        self.state.borrow().bounds
    }

    fn set_parent_origin(&self, origin: Point<DevicePixels>) -> anyhow::Result<()> {
        let mut state = self.state.borrow_mut();
        state.parent_origin = origin;
        state.apply()
    }

    fn set_visible(&self, visible: bool) -> anyhow::Result<()> {
        let mut state = self.state.borrow_mut();
        state.visible = visible;
        state.apply()
    }

    fn platform_handle(&self) -> anyhow::Result<Box<dyn Any>> {
        let window = NonZeroU32::new(self.state.borrow().x_window)
            .context("native surface has no X11 window")?;
        let mut handle = rwh::XcbWindowHandle::new(window);
        handle.visual_id = NonZeroU32::new(self.visual_id);
        Ok(Box::new(rwh::RawWindowHandle::Xcb(handle)))
    }
}

impl Drop for X11NativeSurface {
    fn drop(&mut self) {
        self.state.borrow_mut().destroy();
    }
}

impl X11Composition {
    fn create_native_surface(&mut self) -> anyhow::Result<Rc<RefCell<X11NativeSurfaceState>>> {
        let window = self.xcb.generate_id()?;
        check_reply(
            || "X11 CreateWindow for a native surface failed.",
            self.xcb.create_window(
                self.depth,
                window,
                self.x_window,
                0,
                0,
                1,
                1,
                0,
                xproto::WindowClass::INPUT_OUTPUT,
                self.visual_id,
                &xproto::CreateWindowAux::new(),
            ),
        )?;
        // Selecting pointer events here keeps them from propagating to the
        // GPUI window, which would otherwise treat them as its own input.
        let selected = check_reply(
            || "X11 XiSelectEvents for a native surface failed.",
            self.xcb.xinput_xi_select_events(
                window,
                &[xinput::EventMask {
                    deviceid: XINPUT_ALL_DEVICE_GROUPS,
                    mask: vec![
                        xinput::XIEventMask::MOTION
                            | xinput::XIEventMask::BUTTON_PRESS
                            | xinput::XIEventMask::BUTTON_RELEASE,
                    ],
                }],
            ),
        );
        if let Err(error) = selected {
            check_reply(
                || "X11 DestroyWindow for a native surface failed.",
                self.xcb.destroy_window(window),
            )
            .log_err();
            return Err(error);
        }
        xcb_flush(&self.xcb);
        let state = Rc::new(RefCell::new(X11NativeSurfaceState {
            xcb: self.xcb.clone(),
            x_window: window,
            parent_window: self.x_window,
            bounds: Bounds::default(),
            parent_origin: Point::default(),
            visible: true,
            mapped: false,
            occluders: Vec::new(),
            applied_shape: None,
            destroyed: false,
        }));
        self.native_surfaces.push(Rc::downgrade(&state));
        Ok(state)
    }

    fn native_surface(
        &mut self,
        attachment: &dyn PlatformSurfaceAttachment,
    ) -> anyhow::Result<Rc<RefCell<X11NativeSurfaceState>>> {
        let handle = attachment
            .platform_handle()?
            .downcast::<rwh::RawWindowHandle>()
            .map_err(|_| anyhow!("native surface is not backed by an X11 window"))?;
        let rwh::RawWindowHandle::Xcb(handle) = *handle else {
            anyhow::bail!("native surface is not backed by an X11 window");
        };
        self.native_surfaces
            .retain(|surface| surface.strong_count() > 0);
        self.native_surfaces
            .iter()
            .filter_map(Weak::upgrade)
            .find(|surface| surface.borrow().x_window == handle.window.get())
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

        let mut origins = FxHashMap::default();
        for surface in &order {
            origins.insert(surface.id, surface.window_origin);
        }
        origins.insert(base, Point::default());

        for (index, surface) in order.iter().enumerate() {
            let Some(state) = native_states.get(&surface.id) else {
                continue;
            };
            // A native child can only nest inside another native child; GPUI
            // surfaces are all drawn into the window itself.
            let (parent_window, parent_origin) = surface
                .parent
                .and_then(|parent| {
                    native_states.get(&parent).map(|parent_state| {
                        (
                            parent_state.borrow().x_window,
                            origins.get(&parent).copied().unwrap_or_default(),
                        )
                    })
                })
                .unwrap_or((self.x_window, Point::default()));

            let occluders = order[index + 1..]
                .iter()
                .filter(|above| above.id != base)
                .filter_map(|above| self.occluders.get(&above.id))
                .flatten()
                .copied()
                .collect::<Vec<_>>();

            let mut state = state.borrow_mut();
            if state.parent_window != parent_window {
                check_reply(
                    || "X11 ReparentWindow for a native surface failed.",
                    self.xcb
                        .reparent_window(state.x_window, parent_window, 0, 0),
                )?;
                state.parent_window = parent_window;
                // Reparenting unmaps a mapped window.
                state.mapped = false;
            }
            state.parent_origin = parent_origin;
            if let Some(bounds) = surface.window_bounds {
                state.bounds = bounds;
            }
            state.occluders = occluders;
            // Raising each surface in bottom-to-top order leaves siblings
            // stacked in composition order.
            check_reply(
                || "X11 ConfigureWindow stacking for a native surface failed.",
                self.xcb.configure_window(
                    state.x_window,
                    &xproto::ConfigureWindowAux::new().stack_mode(xproto::StackMode::ABOVE),
                ),
            )?;
            state.apply()?;
        }
        Ok(())
    }

    fn destroy(&mut self) {
        for surface in self
            .native_surfaces
            .drain(..)
            .filter_map(|surface| surface.upgrade())
        {
            surface.borrow_mut().destroy();
        }
        self.order.clear();
        self.occluders.clear();
        self.base_surface = None;
        if let Overlay::Supported {
            colormap,
            window,
            spare_renderer,
            ..
        } = std::mem::replace(&mut self.overlay, Overlay::Unknown)
        {
            for mut renderer in window
                .map(|window| window.destroy(&self.xcb))
                .into_iter()
                .chain(spare_renderer)
            {
                renderer.destroy();
            }
            check_reply(
                || "X11 FreeColormap for the overlay window failed.",
                self.xcb.free_colormap(colormap),
            )
            .log_err();
            xcb_flush(&self.xcb);
        }
    }

    /// The GPUI surfaces stacked above a native surface, drawn into the overlay
    /// window when the window has one.
    fn surfaces_above_native(&self) -> FxHashSet<CompositionSurfaceId> {
        let Some(first_native) = self.order.iter().position(|surface| {
            matches!(
                surface.content,
                PlatformCompositionSurfaceContent::Native(_)
                    | PlatformCompositionSurfaceContent::ExternalGpu(_)
            )
        }) else {
            return FxHashSet::default();
        };
        self.order[first_native + 1..]
            .iter()
            .filter(|surface| matches!(surface.content, PlatformCompositionSurfaceContent::Gpui))
            .map(|surface| surface.id)
            .collect()
    }

    /// Whether GPUI content above native surfaces can go into an overlay
    /// window, deciding it the first time it is asked.
    fn overlay_supported(&mut self) -> bool {
        if matches!(self.overlay, Overlay::Unknown) {
            self.overlay = match self.detect_overlay_support() {
                Ok(Some((root, colormap))) => Overlay::Supported {
                    root,
                    colormap,
                    window: None,
                    spare_renderer: None,
                },
                Ok(None) => Overlay::Unsupported,
                Err(error) => {
                    log::warn!("X11 composition overlay window unavailable: {error:#}");
                    Overlay::Unsupported
                }
            };
        }
        matches!(self.overlay, Overlay::Supported { .. })
    }

    /// The root window and a colormap for an overlay window, if the window has
    /// an ARGB visual and a compositing manager blends such windows.
    fn detect_overlay_support(&self) -> anyhow::Result<Option<(xproto::Window, xproto::Colormap)>> {
        if self.depth != 32 {
            return Ok(None);
        }
        let root = get_reply(
            || "X11 GetGeometry for the composition window failed.",
            self.xcb.get_geometry(self.x_window),
        )?
        .root;
        let screen = self
            .xcb
            .setup()
            .roots
            .iter()
            .position(|screen| screen.root == root)
            .context("composition window is on an unknown screen")?;
        // XWayland always runs under a compositor, which may not claim the
        // X11 compositing manager selection.
        let composited = self
            .xcb
            .extension_information("XWAYLAND")
            .context("X11 QueryExtension for XWAYLAND failed")?
            .is_some()
            || {
                let atom = get_reply(
                    || "X11 InternAtom for the compositing manager selection failed.",
                    self.xcb
                        .intern_atom(false, format!("_NET_WM_CM_S{screen}").as_bytes()),
                )?
                .atom;
                get_reply(
                    || "X11 GetSelectionOwner for the compositing manager failed.",
                    self.xcb.get_selection_owner(atom),
                )?
                .owner
                    != x11rb::NONE
            };
        if !composited {
            return Ok(None);
        }
        let colormap = self.xcb.generate_id()?;
        check_reply(
            || "X11 CreateColormap for the overlay window failed.",
            self.xcb
                .create_colormap(xproto::ColormapAlloc::NONE, colormap, root, self.visual_id),
        )?;
        Ok(Some((root, colormap)))
    }

    /// Draws `scene`, the GPUI content above the native surfaces, into the
    /// overlay window, creating it on first use and unmapping it when the
    /// scene is empty.
    fn draw_overlay(&mut self, window: &X11Window, scene: &Scene) -> anyhow::Result<()> {
        let Overlay::Supported {
            root,
            colormap,
            window: overlay,
            spare_renderer,
        } = &mut self.overlay
        else {
            return Ok(());
        };
        if scene.is_empty() {
            if let Some(overlay) = overlay.take() {
                *spare_renderer = Some(overlay.destroy(&self.xcb));
                xcb_flush(&self.xcb);
            }
            return Ok(());
        }

        let state = window.0.state.borrow();
        let Some(window_renderer) = state.renderer.as_ref() else {
            return Ok(());
        };
        if window_renderer.device_lost() {
            return Ok(());
        }
        let size = window_renderer.viewport_size();
        if overlay
            .as_ref()
            .is_some_and(|overlay| overlay.renderer.device_lost())
            && let Some(lost) = overlay.take()
        {
            lost.destroy(&self.xcb).destroy();
        }
        if spare_renderer
            .as_ref()
            .is_some_and(|renderer| renderer.device_lost())
            && let Some(mut lost) = spare_renderer.take()
        {
            lost.destroy();
        }
        let overlay = match overlay {
            Some(overlay) => overlay,
            None => {
                let created = OverlayWindow::new(
                    &self.xcb,
                    window_renderer,
                    spare_renderer.take(),
                    self.x_window,
                    *root,
                    *colormap,
                    self.depth,
                    self.visual_id,
                    size,
                )?;
                INPUT_WINDOWS.with(|windows| {
                    windows
                        .borrow_mut()
                        .insert(created.x_window, window.0.clone())
                });
                overlay.insert(created)
            }
        };
        drop(state);

        let origin = get_reply(
            || "X11 TranslateCoordinates for the overlay window failed.",
            self.xcb.translate_coordinates(self.x_window, *root, 0, 0),
        )?;
        overlay.set_geometry(
            &self.xcb,
            Point::new(
                DevicePixels(origin.dst_x.into()),
                DevicePixels(origin.dst_y.into()),
            ),
            size,
        )?;
        overlay.set_input(&self.xcb, scene_occluders(scene, false))?;
        overlay.map(&self.xcb)?;
        overlay.raise_above(&self.xcb, self.x_window);
        overlay.renderer.draw(scene);
        xcb_flush(&self.xcb);
        Ok(())
    }

    /// Destroys the overlay window rather than unmapping it, keeping its
    /// renderer for the next one: compositors that raise a window with only
    /// its mapped transients (Hyprland) would leave a remapped overlay window
    /// below its floating parent, while a new window stacks on top.
    fn hide_overlay(&mut self) {
        if let Overlay::Supported {
            window,
            spare_renderer,
            ..
        } = &mut self.overlay
            && let Some(overlay) = window.take()
        {
            *spare_renderer = Some(overlay.destroy(&self.xcb));
            xcb_flush(&self.xcb);
        }
    }
}

thread_local! {
    /// The windows that overlay windows cover, by overlay window, so input on
    /// an overlay window is delivered to the window below it.
    static INPUT_WINDOWS: RefCell<FxHashMap<xproto::Window, X11WindowStatePtr>> =
        RefCell::default();
}

/// `X11Client::get_window`'s fallback for an overlay window: the window it
/// covers, which shares its coordinate space.
pub(crate) fn input_window(x_window: xproto::Window) -> Option<X11WindowStatePtr> {
    INPUT_WINDOWS.with(|windows| windows.borrow().get(&x_window).cloned())
}

/// Whether a window draws the GPUI content above its native surfaces into an
/// overlay window.
enum Overlay {
    Unknown,
    Unsupported,
    Supported {
        root: xproto::Window,
        colormap: xproto::Colormap,
        window: Option<OverlayWindow>,
        /// The renderer of the last destroyed overlay window, reused by the
        /// next one: its pipelines take tens of milliseconds to create.
        spare_renderer: Option<WgpuRenderer>,
    },
}

/// A transparent override-redirect window kept over a window's area, drawn by
/// a renderer sharing the window's sprite atlas.
struct OverlayWindow {
    x_window: xproto::Window,
    renderer: WgpuRenderer,
    origin: Point<DevicePixels>,
    size: Size<DevicePixels>,
    input: Option<Vec<Bounds<DevicePixels>>>,
    mapped: bool,
}

impl OverlayWindow {
    fn new(
        xcb: &Rc<XCBConnection>,
        window_renderer: &WgpuRenderer,
        spare_renderer: Option<WgpuRenderer>,
        parent: xproto::Window,
        root: xproto::Window,
        colormap: xproto::Colormap,
        depth: u8,
        visual_id: u32,
        size: Size<DevicePixels>,
    ) -> anyhow::Result<Self> {
        let x_window = xcb.generate_id()?;
        check_reply(
            || "X11 CreateWindow for the overlay window failed.",
            xcb.create_window(
                depth,
                x_window,
                root,
                0,
                0,
                size.width.0.max(1) as u16,
                size.height.0.max(1) as u16,
                0,
                xproto::WindowClass::INPUT_OUTPUT,
                visual_id,
                &xproto::CreateWindowAux::new()
                    .background_pixel(0)
                    .border_pixel(0)
                    .colormap(colormap)
                    .override_redirect(1),
            ),
        )?;
        let mut overlay = Self {
            x_window,
            renderer: match Self::create_renderer(
                xcb,
                window_renderer,
                spare_renderer,
                x_window,
                visual_id,
                size,
            ) {
                Ok(renderer) => renderer,
                Err(error) => {
                    check_reply(
                        || "X11 DestroyWindow for the overlay window failed.",
                        xcb.destroy_window(x_window),
                    )
                    .log_err();
                    return Err(error);
                }
            },
            origin: Point::default(),
            size,
            input: None,
            mapped: false,
        };
        overlay.set_popup_hints(xcb, parent)?;
        // An empty input shape until the first frame sets it from content.
        overlay.set_input(xcb, Vec::new())?;
        check_reply(
            || "X11 XiSelectEvents for the overlay window failed.",
            xcb.xinput_xi_select_events(
                x_window,
                &[xinput::EventMask {
                    deviceid: XINPUT_ALL_DEVICE_GROUPS,
                    mask: vec![
                        xinput::XIEventMask::MOTION
                            | xinput::XIEventMask::BUTTON_PRESS
                            | xinput::XIEventMask::BUTTON_RELEASE
                            | xinput::XIEventMask::ENTER
                            | xinput::XIEventMask::LEAVE,
                    ],
                }],
            ),
        )?;
        Ok(overlay)
    }

    /// Marks the window as a dropdown menu of `parent` that takes no input
    /// focus, as X11 popup menus are. Compositors that manage override-redirect
    /// windows otherwise focus it, which deactivates `parent` and closes GPUI's
    /// menus and popovers, or raise `parent` above it when `parent` floats.
    fn set_popup_hints(&self, xcb: &XCBConnection, parent: xproto::Window) -> anyhow::Result<()> {
        use x11rb::wrapper::ConnectionExt as _;

        let atom = |name: &str| -> anyhow::Result<xproto::Atom> {
            Ok(get_reply(
                || format!("X11 InternAtom {name} for the overlay window failed."),
                xcb.intern_atom(false, name.as_bytes()),
            )?
            .atom)
        };
        check_reply(
            || "X11 ChangeProperty _NET_WM_WINDOW_TYPE for the overlay window failed.",
            xcb.change_property32(
                xproto::PropMode::REPLACE,
                self.x_window,
                atom("_NET_WM_WINDOW_TYPE")?,
                xproto::AtomEnum::ATOM,
                &[atom("_NET_WM_WINDOW_TYPE_DROPDOWN_MENU")?],
            ),
        )?;
        // WM_HINTS with only the input hint set, to false.
        const INPUT_HINT: u32 = 1;
        check_reply(
            || "X11 ChangeProperty WM_HINTS for the overlay window failed.",
            xcb.change_property32(
                xproto::PropMode::REPLACE,
                self.x_window,
                xproto::AtomEnum::WM_HINTS,
                xproto::AtomEnum::WM_HINTS,
                &[INPUT_HINT, 0, 0, 0, 0, 0, 0, 0, 0],
            ),
        )?;
        check_reply(
            || "X11 ChangeProperty WM_TRANSIENT_FOR for the overlay window failed.",
            xcb.change_property32(
                xproto::PropMode::REPLACE,
                self.x_window,
                xproto::AtomEnum::WM_TRANSIENT_FOR,
                xproto::AtomEnum::WINDOW,
                &[parent],
            ),
        )
    }

    fn create_renderer(
        xcb: &Rc<XCBConnection>,
        window_renderer: &WgpuRenderer,
        spare_renderer: Option<WgpuRenderer>,
        x_window: xproto::Window,
        visual_id: u32,
        size: Size<DevicePixels>,
    ) -> anyhow::Result<WgpuRenderer> {
        let raw_window = OverlayRawWindow {
            connection: xcb.clone(),
            x_window,
            visual_id,
        };
        let config = WgpuSurfaceConfig {
            size,
            transparent: true,
            preferred_present_mode: Some(wgpu::PresentMode::Mailbox),
        };
        match spare_renderer {
            Some(mut renderer) => {
                renderer.replace_surface_sharing_context(&raw_window, config)?;
                Ok(renderer)
            }
            None => window_renderer.new_sharing_atlas(&raw_window, config),
        }
    }

    fn set_geometry(
        &mut self,
        xcb: &XCBConnection,
        origin: Point<DevicePixels>,
        size: Size<DevicePixels>,
    ) -> anyhow::Result<()> {
        if (origin, size) == (self.origin, self.size) && self.mapped {
            return Ok(());
        }
        check_reply(
            || "X11 ConfigureWindow for the overlay window failed.",
            xcb.configure_window(
                self.x_window,
                &xproto::ConfigureWindowAux::new()
                    .x(origin.x.0)
                    .y(origin.y.0)
                    .width(size.width.0.max(1) as u32)
                    .height(size.height.0.max(1) as u32),
            ),
        )?;
        if size != self.size {
            self.renderer.update_drawable_size(size);
        }
        self.origin = origin;
        self.size = size;
        Ok(())
    }

    fn set_input(
        &mut self,
        xcb: &XCBConnection,
        input: Vec<Bounds<DevicePixels>>,
    ) -> anyhow::Result<()> {
        if self.input.as_ref() == Some(&input) {
            return Ok(());
        }
        check_reply(
            || "X11 ShapeRectangles input for the overlay window failed.",
            xcb.shape_rectangles(
                shape::SO::SET,
                shape::SK::INPUT,
                xproto::ClipOrdering::UNSORTED,
                self.x_window,
                0,
                0,
                &input.iter().copied().map(x11_rectangle).collect::<Vec<_>>(),
            ),
        )?;
        self.input = Some(input);
        Ok(())
    }

    fn map(&mut self, xcb: &XCBConnection) -> anyhow::Result<()> {
        if self.mapped {
            return Ok(());
        }
        check_reply(
            || "X11 MapWindow for the overlay window failed.",
            xcb.map_window(self.x_window),
        )?;
        xcb_flush(xcb);
        self.mapped = true;
        Ok(())
    }

    /// Restacks the window just above `parent`, which the compositor raises
    /// over it when `parent` is focused or clicked.
    fn raise_above(&self, xcb: &XCBConnection, parent: xproto::Window) {
        // Fails harmlessly when a window manager has reparented `parent`, so
        // the two are no longer siblings.
        check_reply(
            || "X11 ConfigureWindow stacking for the overlay window failed.",
            xcb.configure_window(
                self.x_window,
                &xproto::ConfigureWindowAux::new()
                    .sibling(parent)
                    .stack_mode(xproto::StackMode::ABOVE),
            ),
        )
        .log_err();
    }

    /// Destroys the window, returning its renderer with its surface released.
    fn destroy(mut self, xcb: &XCBConnection) -> WgpuRenderer {
        INPUT_WINDOWS.with(|windows| windows.borrow_mut().remove(&self.x_window));
        // Release the wgpu surface before the window it presents to.
        self.renderer.unconfigure_surface();
        check_reply(
            || "X11 DestroyWindow for the overlay window failed.",
            xcb.destroy_window(self.x_window),
        )
        .log_err();
        self.renderer
    }
}

/// The overlay window's handle for creating its renderer's surface.
struct OverlayRawWindow {
    connection: Rc<XCBConnection>,
    x_window: xproto::Window,
    visual_id: u32,
}

impl rwh::HasWindowHandle for OverlayRawWindow {
    fn window_handle(&self) -> Result<rwh::WindowHandle<'_>, rwh::HandleError> {
        let _ = &self.connection;
        let window = NonZeroU32::new(self.x_window).ok_or(rwh::HandleError::Unavailable)?;
        let mut handle = rwh::XcbWindowHandle::new(window);
        handle.visual_id = NonZeroU32::new(self.visual_id);
        // SAFETY: The overlay window outlives its renderer, which is destroyed first.
        Ok(unsafe { rwh::WindowHandle::borrow_raw(handle.into()) })
    }
}

/// `PlatformWindow::draw_composed`: draws every GPUI surface into the window
/// in composition order, so later surfaces paint over earlier ones as they
/// would when stacked, and cuts the overlays' content out of the native
/// surfaces below them.
pub(crate) fn draw_composed(window: &X11Window, scene: ComposedScene<'_>) {
    let composition = window.0.state.borrow().fast_composition.0.clone();
    let mut composition = composition.borrow_mut();
    let Some(base_surface) = composition.base_surface else {
        drop(composition);
        window.draw(scene.scene());
        return;
    };

    let track_occluders = composition
        .native_surfaces
        .iter()
        .any(|surface| surface.strong_count() > 0);
    let overlay_surfaces = if track_occluders && composition.overlay_supported() {
        composition.surfaces_above_native()
    } else {
        FxHashSet::default()
    };
    // Content drawn into the overlay window is drawn into the window too, and
    // its opaque parts are cut out of the native surfaces: when the compositor
    // stacks the overlay window below the window (Hyprland renders a pinned
    // window above every other), menus and dialogs still show, only without
    // the shadows and backdrops blended over native content.
    let mut window_scene = Scene::default();
    let mut overlay_scene = Scene::default();
    let mut occluders = FxHashMap::default();
    for layer in scene.layers() {
        let in_overlay = overlay_surfaces.contains(&layer.surface);
        let cuts_out = track_occluders && layer.surface != base_surface;
        let mut layer_scene = Scene::default();
        for range in &layer.ranges {
            let replayed = window_scene
                .replay_balanced(range.clone(), scene.scene())
                .and_then(|()| {
                    if in_overlay {
                        overlay_scene.replay_balanced(range.clone(), scene.scene())
                    } else {
                        Ok(())
                    }
                })
                .and_then(|()| {
                    if cuts_out {
                        layer_scene.replay_balanced(range.clone(), scene.scene())
                    } else {
                        Ok(())
                    }
                });
            if let Err(error) = replayed {
                log::error!("replaying X11 composition scene: {error:#}");
                return;
            }
        }
        if cuts_out {
            layer_scene.finish();
            occluders.insert(layer.surface, scene_occluders(&layer_scene, in_overlay));
        }
    }
    window_scene.finish();
    overlay_scene.finish();

    if occluders != composition.occluders {
        composition.occluders = occluders;
        composition.apply_order().log_err();
    }
    if overlay_surfaces.is_empty() {
        composition.hide_overlay();
    } else if let Err(error) = composition.draw_overlay(window, &overlay_scene) {
        log::error!("drawing the X11 composition overlay window: {error:#}");
    }
    drop(composition);
    window.draw(&window_scene);
}

/// `PlatformWindow::enable_window_composition`: composition needs the SHAPE
/// extension to cut overlays out of native surfaces.
pub(crate) fn enable_window_composition(window: &X11Window) -> anyhow::Result<()> {
    let shape = window
        .0
        .xcb
        .extension_information(shape::X11_EXTENSION_NAME)
        .context("X11 QueryExtension for SHAPE failed")?;
    anyhow::ensure!(
        shape.is_some(),
        "the X11 server does not support the SHAPE extension"
    );
    Ok(())
}

/// `PlatformWindow::create_native_surface`: a child window of the window.
pub(crate) fn create_native_surface(
    window: &X11Window,
) -> anyhow::Result<Rc<dyn PlatformSurfaceAttachment>> {
    enable_window_composition(window)?;
    let composition = window.0.state.borrow().fast_composition.0.clone();
    let mut composition = composition.borrow_mut();
    let state = composition.create_native_surface()?;
    Ok(Rc::new(X11NativeSurface {
        state,
        visual_id: composition.visual_id,
    }))
}

/// `PlatformWindow::set_composition_order`: reparents, restacks and reshapes
/// the native surfaces in `surfaces`' order.
pub(crate) fn set_composition_order(
    window: &X11Window,
    surfaces: &[PlatformCompositionSurface],
) -> anyhow::Result<()> {
    let base_surface = surfaces
        .iter()
        .find_map(|surface| match surface.content {
            PlatformCompositionSurfaceContent::Gpui => Some(surface.id),
            PlatformCompositionSurfaceContent::Native(_)
            | PlatformCompositionSurfaceContent::ExternalGpu(_) => None,
        })
        .context("composition has no GPUI base surface")?;
    let composition = window.0.state.borrow().fast_composition.0.clone();
    let mut composition = composition.borrow_mut();
    composition.base_surface = Some(base_surface);
    composition.order = surfaces.to_vec();
    composition
        .occluders
        .retain(|id, _| surfaces.iter().any(|surface| surface.id == *id));
    composition.apply_order()
}

#[cfg(test)]
mod tests {
    use super::scene_occluders;
    use gpui::{
        Bounds, ContentMask, DevicePixels, Quad, ScaledPixels, Scene, Shadow, black, point, size,
    };

    fn quad(bounds: Bounds<ScaledPixels>, mask: Bounds<ScaledPixels>) -> Quad {
        Quad {
            bounds,
            content_mask: ContentMask { bounds: mask },
            ..Default::default()
        }
    }

    fn scaled(x: f32, y: f32, width: f32, height: f32) -> Bounds<ScaledPixels> {
        Bounds::new(
            point(ScaledPixels(x), ScaledPixels(y)),
            size(ScaledPixels(width), ScaledPixels(height)),
        )
    }

    fn device(x: i32, y: i32, width: i32, height: i32) -> Bounds<DevicePixels> {
        Bounds::new(
            point(DevicePixels(x), DevicePixels(y)),
            size(DevicePixels(width), DevicePixels(height)),
        )
    }

    #[test]
    fn scene_occluders_skip_shadows_and_covered_rectangles() {
        let everything = scaled(0., 0., 1000., 1000.);
        let mut scene = Scene::default();
        scene.insert_primitive(Shadow {
            order: 0,
            blur_radius: ScaledPixels(24.),
            bounds: scaled(500., 500., 100., 100.),
            corner_radii: Default::default(),
            content_mask: ContentMask { bounds: everything },
            color: Default::default(),
            element_bounds: scaled(500., 500., 100., 100.),
            element_corner_radii: Default::default(),
            inset: 0,
            pad: 0,
        });
        scene.insert_primitive(quad(scaled(10., 10., 100., 50.), everything));
        scene.insert_primitive(quad(scaled(20.5, 20.5, 10., 10.), everything));
        scene.insert_primitive(quad(
            scaled(200., 0., 100., 100.),
            scaled(250., 0., 20., 40.),
        ));
        scene.finish();

        let mut occluders = scene_occluders(&scene, false);
        occluders.sort_by_key(|bounds| bounds.origin.x);
        assert_eq!(occluders, [device(10, 10, 100, 50), device(250, 0, 20, 40)]);
    }

    #[test]
    fn opaque_scene_occluders_skip_translucent_backgrounds() {
        let everything = scaled(0., 0., 1000., 1000.);
        let mut scene = Scene::default();
        // A dialog's translucent backdrop over the whole window, and its panel.
        scene.insert_primitive(Quad {
            background: black().opacity(0.4).into(),
            ..quad(everything, everything)
        });
        scene.insert_primitive(Quad {
            background: black().into(),
            ..quad(scaled(300., 200., 400., 300.), everything)
        });
        scene.finish();

        assert_eq!(scene_occluders(&scene, true), [device(300, 200, 400, 300)]);
        assert_eq!(scene_occluders(&scene, false), [device(0, 0, 1000, 1000)]);
    }
}
