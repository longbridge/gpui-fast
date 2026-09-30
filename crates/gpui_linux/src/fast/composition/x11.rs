//! Window composition on X11 (zed-industries/zed#62379).
//!
//! X11 draws every GPUI composition surface into the window itself: a child
//! window with the same ARGB visual as its parent replaces the parent's pixels
//! instead of blending over them, so GPUI overlays cannot be child windows.
//! Native surfaces are child windows, and the GPUI content stacked above one
//! is cut out of its shape (the SHAPE extension) so the window's pixels and
//! input show through.

use std::{
    any::Any,
    cell::RefCell,
    num::NonZeroU32,
    rc::{Rc, Weak},
};

use anyhow::{Context as _, anyhow};
use collections::FxHashMap;
use gpui::{
    Bounds, ComposedScene, CompositionSurfaceId, ContentMask, DevicePixels,
    PlatformCompositionSurface, PlatformCompositionSurfaceContent, PlatformSurfaceAttachment,
    PlatformWindow, Point, ScaledPixels, Scene,
};
use gpui_util::ResultExt as _;
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

use crate::linux::{X11Window, XINPUT_ALL_DEVICE_GROUPS, check_reply, xcb_flush};

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
fn scene_occluders(scene: &Scene) -> Vec<Bounds<DevicePixels>> {
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
    let mut window_scene = Scene::default();
    let mut occluders = FxHashMap::default();
    for layer in scene.layers() {
        let mut layer_scene = Scene::default();
        for range in &layer.ranges {
            let replayed = window_scene
                .replay_balanced(range.clone(), scene.scene())
                .and_then(|()| {
                    if track_occluders && layer.surface != base_surface {
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
        if track_occluders && layer.surface != base_surface {
            layer_scene.finish();
            occluders.insert(layer.surface, scene_occluders(&layer_scene));
        }
    }
    window_scene.finish();

    if occluders != composition.occluders {
        composition.occluders = occluders;
        composition.apply_order().log_err();
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
    use gpui::{Bounds, ContentMask, DevicePixels, Quad, ScaledPixels, Scene, Shadow, point, size};

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

        let mut occluders = scene_occluders(&scene);
        occluders.sort_by_key(|bounds| bounds.origin.x);
        assert_eq!(occluders, [device(10, 10, 100, 50), device(250, 0, 20, 40)]);
    }
}
