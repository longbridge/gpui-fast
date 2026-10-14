//! Window composition: native views between GPUI's content and its overlays.
//!
//! Ported from zed-industries/zed#62379. A window paints one scene, as
//! always, but can present it on several ordered surfaces, so that a
//! platform-native view (a `WKWebView`, a WebView2, a Wayland subsurface fed
//! by another GPU producer) sits above GPUI's base content and below its
//! overlays — popovers, dialogs, menus, tooltips:
//!
//! ```text
//! front  GPUI overlay   deferred and window-level draws
//!        native surface  WKWebView / WebView2 / wl_subsurface / X11 child
//! back   GPUI base       the root view
//! ```
//!
//! [`CompositionTree`] holds each surface's identity, parent and sibling
//! order. The frame records, as it paints, which GPUI surface each stretch
//! of the scene targets ([`SurfaceStarts`]); presenting splits the scene
//! along those stretches into a [`ComposedScene`], and the platform draws
//! each surface's part onto its own layer, visual or subsurface.
//!
//! Composition is opt-in: until [`Window::enable_window_composition`] is
//! called, a window presents its scene with `PlatformWindow::draw`, as
//! upstream does.
//!
//! Unlike the upstream pull request, a view drawn again from last frame
//! (every retained view, here) carries the surface switches it made with it:
//! [`reuse_starts`] copies them along with its scene.
//!
//! Also unlike it, a native surface stacks with the element hosting it. One
//! positioned while a deferred draw is prepainted, a web page in a dialog or
//! sheet, is hosted on the overlay surface, so it moves above that surface
//! instead of under the dialog's own background; positioned while the root
//! view is prepainted, it moves back below it. Overlays drawn over a native
//! surface hosted on the overlay surface are then covered by it.

use crate::window::PaintIndex;
use crate::{
    Bounds, DevicePixels, DispatchEventResult, Hitbox, HitboxBehavior, HitboxId, MouseButton,
    MouseExitEvent, MouseMoveEvent, Pixels, PlatformInput, PlatformWindow, Point, Scene, Window,
    scene::PaintOperation,
};
use anyhow::{Context as _, Result, anyhow, bail};
use collections::FxHashMap;
use gpui_util::ResultExt as _;
use slotmap::SlotMap;
use std::{
    any::Any,
    cell::{Cell, RefCell},
    ops::Range,
    rc::Rc,
};

slotmap::new_key_type! {
    /// Stable identity for a surface in a window composition tree.
    pub struct CompositionSurfaceId;
}

/// The renderer or platform facility responsible for a composition surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompositionSurfaceKind {
    /// A surface rendered from a range of GPUI scene operations.
    Gpui,
    /// A surface whose content is supplied by a platform-native view or visual.
    Native,
    /// A surface whose content is supplied by an external GPU producer.
    ExternalGpu,
}

#[derive(Clone)]
struct CompositionSurfaceNode {
    kind: CompositionSurfaceKind,
    parent: Option<CompositionSurfaceId>,
    children: Vec<CompositionSurfaceId>,
}

/// The surfaces of a window, as a forest ordered bottom to top.
#[derive(Clone)]
pub(crate) struct CompositionTree {
    surfaces: SlotMap<CompositionSurfaceId, CompositionSurfaceNode>,
    roots: Vec<CompositionSurfaceId>,
}

impl CompositionTree {
    pub(crate) fn new() -> Self {
        Self {
            surfaces: SlotMap::with_key(),
            roots: Vec::new(),
        }
    }

    /// Adds a surface above its future siblings.
    pub(crate) fn insert(
        &mut self,
        kind: CompositionSurfaceKind,
        parent: Option<CompositionSurfaceId>,
    ) -> Result<CompositionSurfaceId> {
        if let Some(parent) = parent {
            self.surface(parent)?;
        }

        let surface = self.surfaces.insert(CompositionSurfaceNode {
            kind,
            parent,
            children: Vec::new(),
        });
        self.siblings_mut(parent)?.push(surface);
        Ok(surface)
    }

    /// Removes a surface, putting its children where it was.
    pub(crate) fn remove(&mut self, surface: CompositionSurfaceId) -> Result<()> {
        let parent = self.surface(surface)?.parent;
        let children = self.surface(surface)?.children.clone();
        let siblings = self.siblings_mut(parent)?;
        let index = siblings
            .iter()
            .position(|candidate| *candidate == surface)
            .ok_or_else(|| anyhow!("composition surface is missing from its parent"))?;
        siblings.splice(index..=index, children.iter().copied());
        for child in children {
            self.surface_mut(child)?.parent = parent;
        }
        self.surfaces.remove(surface);
        Ok(())
    }

    pub(crate) fn place_above(
        &mut self,
        surface: CompositionSurfaceId,
        sibling: CompositionSurfaceId,
    ) -> Result<()> {
        self.place_relative(surface, sibling, true)
    }

    pub(crate) fn place_below(
        &mut self,
        surface: CompositionSurfaceId,
        sibling: CompositionSurfaceId,
    ) -> Result<()> {
        self.place_relative(surface, sibling, false)
    }

    /// Moves a surface under `parent`, above the children it has.
    pub(crate) fn reparent(
        &mut self,
        surface: CompositionSurfaceId,
        parent: Option<CompositionSurfaceId>,
    ) -> Result<()> {
        self.surface(surface)?;
        if let Some(parent) = parent {
            self.surface(parent)?;
            if parent == surface || self.is_descendant(parent, surface)? {
                bail!("composition surfaces cannot contain themselves");
            }
        }

        let previous_parent = self.surface(surface)?.parent;
        self.siblings_mut(previous_parent)?
            .retain(|candidate| *candidate != surface);
        self.surface_mut(surface)?.parent = parent;
        self.siblings_mut(parent)?.push(surface);
        Ok(())
    }

    pub(crate) fn kind(&self, surface: CompositionSurfaceId) -> Result<CompositionSurfaceKind> {
        Ok(self.surface(surface)?.kind)
    }

    pub(crate) fn parent(
        &self,
        surface: CompositionSurfaceId,
    ) -> Result<Option<CompositionSurfaceId>> {
        Ok(self.surface(surface)?.parent)
    }

    /// The children of `parent`, or the roots when it is `None`, bottom to top.
    pub(crate) fn children(
        &self,
        parent: Option<CompositionSurfaceId>,
    ) -> Result<&[CompositionSurfaceId]> {
        match parent {
            Some(parent) => Ok(&self.surface(parent)?.children),
            None => Ok(&self.roots),
        }
    }

    /// Every surface, parents before their children, bottom to top.
    pub(crate) fn flattened(&self) -> Vec<CompositionSurfaceId> {
        let mut surfaces = Vec::with_capacity(self.surfaces.len());
        for root in &self.roots {
            self.flatten_into(*root, &mut surfaces);
        }
        surfaces
    }

    fn place_relative(
        &mut self,
        surface: CompositionSurfaceId,
        sibling: CompositionSurfaceId,
        above: bool,
    ) -> Result<()> {
        if surface == sibling {
            bail!("a composition surface cannot be ordered relative to itself");
        }
        let parent = self.surface(surface)?.parent;
        if self.surface(sibling)?.parent != parent {
            bail!("composition surfaces must share a parent to be reordered");
        }

        let siblings = self.siblings_mut(parent)?;
        siblings.retain(|candidate| *candidate != surface);
        let sibling_index = siblings
            .iter()
            .position(|candidate| *candidate == sibling)
            .ok_or_else(|| anyhow!("composition sibling is missing from its parent"))?;
        let index = sibling_index + usize::from(above);
        siblings.insert(index, surface);
        Ok(())
    }

    fn is_descendant(
        &self,
        candidate: CompositionSurfaceId,
        ancestor: CompositionSurfaceId,
    ) -> Result<bool> {
        let mut parent = Some(candidate);
        while let Some(surface) = parent {
            if surface == ancestor {
                return Ok(true);
            }
            parent = self.surface(surface)?.parent;
        }
        Ok(false)
    }

    fn surface(&self, surface: CompositionSurfaceId) -> Result<&CompositionSurfaceNode> {
        self.surfaces
            .get(surface)
            .ok_or_else(|| anyhow!("composition surface does not exist"))
    }

    fn surface_mut(
        &mut self,
        surface: CompositionSurfaceId,
    ) -> Result<&mut CompositionSurfaceNode> {
        self.surfaces
            .get_mut(surface)
            .ok_or_else(|| anyhow!("composition surface does not exist"))
    }

    fn siblings_mut(
        &mut self,
        parent: Option<CompositionSurfaceId>,
    ) -> Result<&mut Vec<CompositionSurfaceId>> {
        match parent {
            Some(parent) => Ok(&mut self.surface_mut(parent)?.children),
            None => Ok(&mut self.roots),
        }
    }

    fn flatten_into(
        &self,
        surface: CompositionSurfaceId,
        flattened: &mut Vec<CompositionSurfaceId>,
    ) {
        flattened.push(surface);
        if let Some(surface) = self.surfaces.get(surface) {
            for child in &surface.children {
                self.flatten_into(*child, flattened);
            }
        }
    }
}

/// Splits a scene of `scene_len` operations along `starts`, the points where
/// painting switched GPUI surface, into one layer per GPUI surface of `tree`,
/// in composition order.
pub(crate) fn composed_scene_layers(
    tree: &CompositionTree,
    starts: &[(CompositionSurfaceId, usize)],
    scene_len: usize,
) -> Result<Vec<ComposedSceneLayer>> {
    let mut ranges_by_surface = FxHashMap::<_, Vec<Range<usize>>>::default();
    for (index, (surface, start)) in starts.iter().enumerate() {
        if tree.kind(*surface)? != CompositionSurfaceKind::Gpui {
            bail!("scene operations target a non-GPUI composition surface");
        }
        let end = starts
            .get(index + 1)
            .map_or(scene_len, |(_, next_start)| *next_start);
        if *start > end || end > scene_len {
            bail!("composition scene ranges are not monotonic");
        }
        if *start < end {
            ranges_by_surface
                .entry(*surface)
                .or_default()
                .push(*start..end);
        }
    }

    let mut layers = Vec::new();
    for surface in tree.flattened() {
        if tree.kind(surface)? == CompositionSurfaceKind::Gpui {
            layers.push(ComposedSceneLayer {
                surface,
                ranges: ranges_by_surface.remove(&surface).unwrap_or_default(),
            });
        }
    }
    Ok(layers)
}

/// A platform attachment participating in a window composition tree.
pub trait PlatformSurfaceAttachment {
    /// Updates the native surface geometry in window-content device pixels.
    fn set_bounds(&self, bounds: Bounds<DevicePixels>) -> Result<()>;
    /// Returns the native surface geometry in window-content device pixels.
    fn bounds(&self) -> Bounds<DevicePixels>;
    /// Updates the window-content origin of the surface's composition parent.
    /// Platform adapters use this to preserve window-coordinate geometry when
    /// a surface is nested.
    fn set_parent_origin(&self, origin: Point<DevicePixels>) -> Result<()>;
    /// Rebinds externally owned content after the platform compositor has been
    /// recreated. Native surfaces managed by GPUI may use the default no-op.
    fn compositor_recreated(&self, _platform_context: &dyn Any) -> Result<()> {
        Ok(())
    }
    /// Registers a callback that reattaches native content when the platform
    /// attachment object changes after compositor recovery.
    fn set_compositor_recreated_callback(
        &self,
        _callback: Rc<dyn Fn(Box<dyn Any>) -> Result<()>>,
    ) -> Result<()> {
        bail!("compositor recreation callbacks are not supported")
    }
    /// Updates whether the native surface participates in composition.
    fn set_visible(&self, visible: bool) -> Result<()>;
    /// Whether GPUI forwards pointer input to a windowless consumer.
    /// OS-managed child views keep their native event path by default.
    fn input_forwarding(&self) -> bool {
        false
    }
    /// Returns the platform attachment object: an `NSView` pointer as `usize`
    /// on macOS, an `IDCompositionVisual` as `windows::core::IUnknown` on
    /// Windows, a `raw_window_handle::RawWindowHandle` on Linux.
    fn platform_handle(&self) -> Result<Box<dyn Any>>;
}

/// The ranges of a GPUI scene rendered onto one composition surface.
#[derive(Clone, Debug)]
pub struct ComposedSceneLayer {
    /// The GPUI composition surface receiving these scene ranges.
    pub surface: CompositionSurfaceId,
    /// Paint-operation ranges belonging to the surface.
    pub ranges: Vec<Range<usize>>,
}

/// A scene split across GPUI surfaces in composition order.
pub struct ComposedScene<'a> {
    scene: &'a Scene,
    layers: Vec<ComposedSceneLayer>,
    input: &'a SurfaceInput,
}

impl<'a> ComposedScene<'a> {
    pub(crate) fn new(
        scene: &'a Scene,
        layers: Vec<ComposedSceneLayer>,
        input: &'a SurfaceInput,
    ) -> Self {
        Self {
            scene,
            layers,
            input,
        }
    }

    /// The prepared, clipped input regions belonging to this GPUI surface.
    /// Regions retain their stacking order and mouse/scroll behavior.
    pub fn layer_hitboxes(&self, layer: &ComposedSceneLayer) -> &[Hitbox] {
        self.input
            .hitboxes
            .get(&layer.surface)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Returns the complete scene containing all GPUI planes.
    pub fn scene(&self) -> &'a Scene {
        self.scene
    }

    /// Returns the GPUI surface layers in composition order.
    pub fn layers(&self) -> &[ComposedSceneLayer] {
        &self.layers
    }

    /// Returns the part of the scene `layer` covers as a finished scene of
    /// its own, ready to draw onto the layer's surface.
    pub fn layer_scene(&self, layer: &ComposedSceneLayer) -> Result<Scene> {
        let mut scene = Scene::default();
        for range in &layer.ranges {
            scene.replay_balanced(range.clone(), self.scene)?;
        }
        scene.finish();
        Ok(scene)
    }
}

/// A platform surface participating in window composition.
#[derive(Clone)]
pub struct PlatformCompositionSurface {
    /// Stable identity in the window composition tree.
    pub id: CompositionSurfaceId,
    /// Parent surface, or `None` for a root surface.
    pub parent: Option<CompositionSurfaceId>,
    /// This surface's origin in window-content device pixels. GPUI surfaces
    /// use the window origin because their backing scenes remain window-based.
    pub window_origin: Point<DevicePixels>,
    /// The parent surface's origin in window-content device pixels.
    pub parent_origin: Point<DevicePixels>,
    /// Native or external surface bounds in window-content device pixels.
    /// GPUI surfaces use the full window and therefore have no explicit bounds.
    pub window_bounds: Option<Bounds<DevicePixels>>,
    /// Renderer or attachment supplying this surface's content.
    pub content: PlatformCompositionSurfaceContent,
}

/// Content attached to a platform composition surface.
#[derive(Clone)]
pub enum PlatformCompositionSurfaceContent {
    /// A surface rendered by GPUI.
    Gpui,
    /// A surface backed by a platform-native view or visual.
    Native(Rc<dyn PlatformSurfaceAttachment>),
    /// A surface backed by an external GPU producer.
    ExternalGpu(Rc<dyn PlatformSurfaceAttachment>),
}

/// What `PlatformWindow::draw_composed` does on a platform without
/// composition: draws the whole scene on the window's one surface.
pub(crate) fn draw_uncomposed<W: PlatformWindow + ?Sized>(window: &W, scene: ComposedScene<'_>) {
    window.draw(scene.scene());
}

/// What the `PlatformWindow` composition methods return on a platform
/// without composition.
pub(crate) fn unsupported<T>(what: &str) -> Result<T> {
    bail!("{what} is not supported on this platform")
}

impl crate::Background {
    /// Returns whether the background covers its bounds with no translucency,
    /// so a platform compositing native content below it can cut that
    /// content out instead of blending it.
    pub fn is_opaque(&self) -> bool {
        match self.tag {
            crate::color::BackgroundTag::Solid => self.solid.is_opaque(),
            crate::color::BackgroundTag::LinearGradient => {
                self.colors.iter().all(|stop| stop.color.is_opaque())
            }
            crate::color::BackgroundTag::PatternSlash
            | crate::color::BackgroundTag::Checkerboard => false,
        }
    }
}

impl Scene {
    /// Returns whether the scene contains no drawable primitives.
    ///
    /// A scene may have paint operations that only open and close empty layers,
    /// so `len() == 0` is not equivalent to having no visible primitives.
    /// Transparent input regions are prepared separately from this scene.
    pub fn is_empty(&self) -> bool {
        self.shadows.is_empty()
            && self.quads.is_empty()
            && self.paths.is_empty()
            && self.underlines.is_empty()
            && self.monochrome_sprites.is_empty()
            && self.subpixel_sprites.is_empty()
            && self.polychrome_sprites.is_empty()
            && self.surfaces.is_empty()
    }

    /// Replays a range as a self-contained scene, restoring any layers that
    /// were active at the start of the range and closing those still active at
    /// its end. The scroll layers whose tiles the range composites come along.
    pub fn replay_balanced(&mut self, range: Range<usize>, previous_scene: &Scene) -> Result<()> {
        anyhow::ensure!(
            range.start <= range.end,
            "scene replay range starts after its end"
        );
        let prefix = previous_scene
            .paint_operations
            .get(..range.start)
            .context("scene replay range starts past the scene")?;
        let operations = previous_scene
            .paint_operations
            .get(range.clone())
            .context("scene replay range ends past the scene")?;
        let mut active_layers = Vec::new();
        for operation in prefix {
            match operation {
                PaintOperation::StartLayer(bounds) => active_layers.push(*bounds),
                PaintOperation::EndLayer => {
                    anyhow::ensure!(
                        active_layers.pop().is_some(),
                        "source scene closes a layer that is not open"
                    );
                }
                PaintOperation::Primitive(_) => {}
            }
        }

        crate::fast::layers::paint::replay_layers(self, range, previous_scene);
        for bounds in &active_layers {
            self.push_layer(*bounds);
        }
        for operation in operations {
            match operation {
                PaintOperation::Primitive(primitive) => self.insert_primitive(primitive.clone()),
                PaintOperation::StartLayer(bounds) => {
                    active_layers.push(*bounds);
                    self.push_layer(*bounds);
                }
                PaintOperation::EndLayer => {
                    anyhow::ensure!(
                        active_layers.pop().is_some(),
                        "scene replay range closes a layer that is not open"
                    );
                    self.pop_layer();
                }
            }
        }
        for _ in active_layers {
            self.pop_layer();
        }
        Ok(())
    }
}

/// Where each stretch of a frame's scene is presented: the GPUI surface
/// painting switched to, and the scene length at the switch.
#[derive(Default)]
pub(crate) struct SurfaceStarts(Vec<(CompositionSurfaceId, usize)>);

/// Composition ownership accompanies existing hitbox identities through retained
/// and scroll-layer replay; no second layout or input tree is built.
#[derive(Default)]
pub(crate) struct SurfaceInput {
    order: Vec<(CompositionSurfaceId, Option<Bounds<Pixels>>)>,
    scope: Option<CompositionSurfaceId>,
    base: Option<CompositionSurfaceId>,
    owners: FxHashMap<HitboxId, CompositionSurfaceId>,
    hitboxes: FxHashMap<CompositionSurfaceId, Vec<Hitbox>>,
}

impl SurfaceInput {
    pub(crate) fn clear(&mut self) {
        self.scope = None;
        self.base = None;
        self.order.clear();
        self.owners.clear();
        for hitboxes in self.hitboxes.values_mut() {
            hitboxes.clear();
        }
    }
}

fn begin_input(window: &mut Window, overlay: bool) {
    let state = window.fast_composition.0.borrow();
    state
        .prepainting
        .set(Some(if overlay { Host::Overlay } else { Host::Base }));
    let input = &mut window.next_frame.fast_composition_input;
    input.base = (!state.platform_surfaces.is_empty()).then_some(state.base_surface);
    input.scope = state.enabled.then_some(if overlay {
        state.overlay_surface
    } else {
        state.base_surface
    });
}

pub(crate) fn input_surface(window: &Window) -> Option<CompositionSurfaceId> {
    window.next_frame.fast_composition_input.scope
}

pub(crate) fn record_input(window: &mut Window, id: HitboxId) {
    let input = &mut window.next_frame.fast_composition_input;
    if let (Some(base), Some(surface)) = (input.base, input.scope) {
        if surface != base {
            input.owners.insert(id, surface);
        }
    }
}

pub(crate) fn finish_input(window: &mut Window) {
    synchronize_geometry(window);
    let scale = window.scale_factor();
    let state = window.fast_composition.0.borrow();
    let input = &mut window.next_frame.fast_composition_input;
    if !state.enabled || state.platform_surfaces.is_empty() {
        input.hitboxes.clear();
        return;
    }
    input
        .hitboxes
        .retain(|surface, _| state.tree.kind(*surface).is_ok());
    for surface in state.input_order.iter().copied() {
        let native = state.platform_surfaces.get(&surface).map(|native| {
            input_bounds(&state, surface, native.bounds().to_pixels(scale), scale)
                .unwrap_or_default()
        });
        input.order.push((surface, native));
    }
    for hitbox in &window.next_frame.hitboxes {
        let surface = input
            .owners
            .get(&hitbox.id)
            .or_else(|| {
                window
                    .rendered_frame
                    .fast_composition_input
                    .owners
                    .get(&hitbox.id)
            })
            .copied()
            .unwrap_or(state.base_surface);
        if !matches!(state.tree.kind(surface), Ok(CompositionSurfaceKind::Gpui)) {
            continue;
        }
        if surface != state.base_surface {
            input.owners.insert(hitbox.id, surface);
        }
        let bounds = hitbox.bounds.intersect(&hitbox.content_mask.bounds);
        let Some(bounds) = input_bounds(&state, surface, bounds, scale) else {
            continue;
        };
        if !bounds.is_empty() {
            let mut clipped = hitbox.clone();
            clipped.bounds = bounds;
            input.hitboxes.entry(surface).or_default().push(clipped);
        }
    }
}

/// Native content blocks GPUI hover and click targets behind it, while keeping
/// the existing GPUI scroll blockers and capture dispatch unchanged.
pub(crate) fn hit_test(
    frame: &crate::window::Frame,
    point: Point<Pixels>,
) -> Option<crate::window::HitTest> {
    let input = &frame.fast_composition_input;
    if input.order.is_empty() {
        return None;
    }
    let mut result = crate::window::HitTest::default();
    let mut hover = None;
    'surfaces: for (surface, native) in input.order.iter().rev() {
        if let Some(bounds) = native {
            if bounds.contains(&point) {
                break;
            }
            continue;
        }
        let Some(hitboxes) = input.hitboxes.get(surface) else {
            continue;
        };
        for hitbox in hitboxes.iter().rev() {
            if hitbox.bounds.contains(&point) {
                result.ids.push(hitbox.id);
                if hover.is_none() && hitbox.behavior == HitboxBehavior::BlockMouseExceptScroll {
                    hover = Some(result.ids.len());
                }
                if hitbox.behavior == HitboxBehavior::BlockMouse {
                    break 'surfaces;
                }
            }
        }
    }
    result.hover_hitbox_count = hover.unwrap_or(result.ids.len());
    Some(result)
}

impl SurfaceStarts {
    pub(crate) fn clear(&mut self) {
        self.0.clear();
    }

    fn current(&self) -> Option<CompositionSurfaceId> {
        self.0.last().map(|(surface, _)| *surface)
    }

    fn switch_to(&mut self, surface: CompositionSurfaceId, scene_len: usize) {
        if self.current() == Some(surface) {
            return;
        }
        self.0.push((surface, scene_len));
    }
}

/// The GPUI surface an element hosting a native surface is drawn on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Host {
    /// The root view, drawn on the base surface.
    Base,
    /// A deferred or window-level draw, drawn on the overlay surface.
    Overlay,
}

/// A window's composition: its surface tree, the platform surfaces in it and
/// whether the window presents through it.
pub(crate) struct WindowCompositionState {
    tree: CompositionTree,
    platform_surfaces: FxHashMap<CompositionSurfaceId, Rc<ManagedPlatformSurface>>,
    base_surface: CompositionSurfaceId,
    overlay_surface: CompositionSurfaceId,
    geometry_dirty: Rc<Cell<bool>>,
    /// What the window is prepainting, while it prepaints a frame.
    prepainting: Rc<Cell<Option<Host>>>,
    enabled: bool,
    input_order: Vec<CompositionSurfaceId>,
    visible: FxHashMap<CompositionSurfaceId, Rc<Cell<bool>>>,
    input_handlers: FxHashMap<CompositionSurfaceId, Rc<RefCell<NativeInputHandler>>>,
    pointer_capture: Option<CompositionSurfaceId>,
    pressed_buttons: Vec<MouseButton>,
    hovered_native: Option<CompositionSurfaceId>,
}

type NativeInputHandler = Box<dyn FnMut(&PlatformInput) -> DispatchEventResult>;

/// The [`Window`] field holding its composition.
pub(crate) struct WindowCompositionHandle(Rc<RefCell<WindowCompositionState>>);

impl Default for WindowCompositionHandle {
    fn default() -> Self {
        let mut tree = CompositionTree::new();
        let base_surface = tree
            .insert(CompositionSurfaceKind::Gpui, None)
            .expect("a root can always be inserted");
        let overlay_surface = tree
            .insert(CompositionSurfaceKind::Gpui, None)
            .expect("a root can always be inserted");
        Self(Rc::new(RefCell::new(WindowCompositionState {
            tree,
            platform_surfaces: FxHashMap::default(),
            base_surface,
            overlay_surface,
            geometry_dirty: Rc::new(Cell::new(false)),
            prepainting: Rc::new(Cell::new(None)),
            enabled: false,
            input_order: Vec::new(),
            visible: FxHashMap::default(),
            input_handlers: FxHashMap::default(),
            pointer_capture: None,
            pressed_buttons: Vec::new(),
            hovered_native: None,
        })))
    }
}

/// A platform surface whose bounds changes mark the window's composition
/// geometry for synchronization at the next present.
struct ManagedPlatformSurface {
    platform_surface: Rc<dyn PlatformSurfaceAttachment>,
    bounds: Cell<Bounds<DevicePixels>>,
    geometry_dirty: Rc<Cell<bool>>,
    visible: Rc<Cell<bool>>,
    input_forwarding: Cell<bool>,
    prepainting: Rc<Cell<Option<Host>>>,
    /// The surface its host was last prepainted on.
    host: Cell<Host>,
    /// Whether `host` changed since the surface was last restacked.
    restack: Cell<bool>,
}

impl ManagedPlatformSurface {
    fn new(
        platform_surface: Rc<dyn PlatformSurfaceAttachment>,
        state: &WindowCompositionState,
    ) -> Self {
        Self {
            bounds: Cell::new(platform_surface.bounds()),
            platform_surface,
            geometry_dirty: state.geometry_dirty.clone(),
            visible: Rc::new(Cell::new(true)),
            input_forwarding: Cell::new(false),
            prepainting: state.prepainting.clone(),
            host: Cell::new(Host::Base),
            restack: Cell::new(false),
        }
    }
}

impl PlatformSurfaceAttachment for ManagedPlatformSurface {
    fn set_bounds(&self, bounds: Bounds<DevicePixels>) -> Result<()> {
        if let Some(host) = self.prepainting.get()
            && self.host.replace(host) != host
        {
            self.restack.set(true);
            self.geometry_dirty.set(true);
        }
        if self.bounds.get() == bounds {
            return Ok(());
        }
        self.platform_surface.set_bounds(bounds)?;
        self.bounds.set(bounds);
        self.geometry_dirty.set(true);
        Ok(())
    }

    fn bounds(&self) -> Bounds<DevicePixels> {
        self.bounds.get()
    }

    fn set_parent_origin(&self, origin: Point<DevicePixels>) -> Result<()> {
        self.platform_surface.set_parent_origin(origin)
    }

    fn compositor_recreated(&self, platform_context: &dyn Any) -> Result<()> {
        self.platform_surface.compositor_recreated(platform_context)
    }

    fn set_compositor_recreated_callback(
        &self,
        callback: Rc<dyn Fn(Box<dyn Any>) -> Result<()>>,
    ) -> Result<()> {
        self.platform_surface
            .set_compositor_recreated_callback(callback)
    }

    fn set_visible(&self, visible: bool) -> Result<()> {
        if self.visible.get() == visible {
            return Ok(());
        }
        self.platform_surface.set_visible(visible)?;
        self.visible.set(visible);
        self.geometry_dirty.set(true);
        Ok(())
    }

    fn input_forwarding(&self) -> bool {
        self.input_forwarding.get() && self.visible.get()
    }

    fn platform_handle(&self) -> Result<Box<dyn Any>> {
        self.platform_surface.platform_handle()
    }
}

struct WindowCompositionSurfaceInner {
    id: CompositionSurfaceId,
    platform_surface: Option<Rc<dyn PlatformSurfaceAttachment>>,
}

/// A stable handle to a surface in a window composition tree.
#[derive(Clone)]
pub struct WindowCompositionSurface(Rc<WindowCompositionSurfaceInner>);

impl WindowCompositionSurface {
    /// Returns this surface's stable identity.
    pub fn id(&self) -> CompositionSurfaceId {
        self.0.id
    }

    /// Returns the platform surface used to attach native content.
    pub fn platform_surface(&self) -> Result<&dyn PlatformSurfaceAttachment> {
        self.0
            .platform_surface
            .as_deref()
            .context("this composition surface is rendered by GPUI")
    }
}

/// Controls the surface tree composited into a window.
pub struct WindowComposition<'a> {
    platform_window: &'a dyn PlatformWindow,
    state: Rc<RefCell<WindowCompositionState>>,
}

impl WindowComposition<'_> {
    /// Registers a cooperating windowless producer's mouse/wheel consumer.
    /// Capture runs first; coordinates are in window-content logical pixels.
    /// OS-managed child views need no handler and keep their platform input path.
    pub fn on_input(
        &self,
        surface: CompositionSurfaceId,
        handler: impl FnMut(&PlatformInput) -> DispatchEventResult + 'static,
    ) -> Result<()> {
        let previous = {
            let mut state = self.state.borrow_mut();
            anyhow::ensure!(
                state.platform_surfaces.contains_key(&surface),
                "input consumer requires a native surface"
            );
            state.platform_surfaces[&surface].input_forwarding.set(true);
            state
                .input_handlers
                .insert(surface, Rc::new(RefCell::new(Box::new(handler))))
        };
        if let Err(error) = self.synchronize_platform_order() {
            let mut state = self.state.borrow_mut();
            state.platform_surfaces[&surface]
                .input_forwarding
                .set(previous.is_some());
            if let Some(previous) = previous {
                state.input_handlers.insert(surface, previous);
            } else {
                state.input_handlers.remove(&surface);
            }
            drop(state);
            self.synchronize_platform_order().log_err();
            return Err(error);
        }
        Ok(())
    }

    /// Returns the GPUI surface initially rendered below native content.
    pub fn base_surface(&self) -> CompositionSurfaceId {
        self.state.borrow().base_surface
    }

    /// Returns the GPUI surface initially rendered above native content.
    pub fn overlay_surface(&self) -> CompositionSurfaceId {
        self.state.borrow().overlay_surface
    }

    /// Creates a platform-native surface as a root of the composition tree,
    /// below the overlay surface.
    pub fn create_native_surface(&self) -> Result<WindowCompositionSurface> {
        let platform_surface = self.platform_window.create_native_surface()?;
        self.register_platform_surface(CompositionSurfaceKind::Native, platform_surface)
    }

    /// Registers an externally produced GPU surface as a root of the
    /// composition tree, below the overlay surface.
    ///
    /// The producer owns the platform attachment and its synchronization. GPUI
    /// owns its placement in the window composition tree.
    pub fn create_external_gpu_surface(
        &self,
        platform_surface: Rc<dyn PlatformSurfaceAttachment>,
    ) -> Result<WindowCompositionSurface> {
        self.register_platform_surface(CompositionSurfaceKind::ExternalGpu, platform_surface)
    }

    /// Creates another GPUI-rendered surface in the composition tree. Paint
    /// onto it with [`Window::with_composition_surface`].
    pub fn create_gpui_surface(
        &self,
        parent: Option<CompositionSurfaceId>,
    ) -> Result<WindowCompositionSurface> {
        let id = self
            .state
            .borrow_mut()
            .tree
            .insert(CompositionSurfaceKind::Gpui, parent)?;
        if let Err(error) = self.synchronize_platform_order() {
            self.state.borrow_mut().tree.remove(id)?;
            self.synchronize_platform_order().log_err();
            return Err(error);
        }
        Ok(surface_handle(id, None))
    }

    /// Places `surface` immediately above `sibling`.
    pub fn place_above(
        &self,
        surface: CompositionSurfaceId,
        sibling: CompositionSurfaceId,
    ) -> Result<()> {
        anyhow::ensure!(
            surface != self.state.borrow().base_surface,
            "the GPUI base surface cannot be reordered"
        );
        self.change_tree(|tree| tree.place_above(surface, sibling))
    }

    /// Places `surface` immediately below `sibling`.
    pub fn place_below(
        &self,
        surface: CompositionSurfaceId,
        sibling: CompositionSurfaceId,
    ) -> Result<()> {
        let base_surface = self.state.borrow().base_surface;
        anyhow::ensure!(
            surface != base_surface && sibling != base_surface,
            "surfaces cannot be placed below or move the GPUI base surface"
        );
        self.change_tree(|tree| tree.place_below(surface, sibling))
    }

    /// Moves `surface` under a new parent, placing it above existing children.
    pub fn reparent(
        &self,
        surface: CompositionSurfaceId,
        parent: Option<CompositionSurfaceId>,
    ) -> Result<()> {
        anyhow::ensure!(
            surface != self.state.borrow().base_surface,
            "the GPUI base surface cannot be reparented"
        );
        self.change_tree(|tree| tree.reparent(surface, parent))
    }

    /// Removes a surface while preserving its children at the removed
    /// surface's previous position.
    pub fn remove_surface(&self, surface: CompositionSurfaceId) -> Result<()> {
        let mut state = self.state.borrow_mut();
        if surface == state.base_surface || surface == state.overlay_surface {
            bail!("the default GPUI composition surfaces cannot be removed");
        }
        let previous_tree = state.tree.clone();
        let platform_surface = state.platform_surfaces.get(&surface).cloned();
        let was_visible = state
            .visible
            .get(&surface)
            .is_none_or(|visible| visible.get());
        if let Some(platform_surface) = &platform_surface {
            platform_surface.set_visible(false)?;
        }
        state.tree.remove(surface)?;
        state.platform_surfaces.remove(&surface);
        drop(state);
        if let Err(error) = self.synchronize_platform_order() {
            let mut state = self.state.borrow_mut();
            state.tree = previous_tree;
            if let Some(platform_surface) = platform_surface {
                platform_surface.set_visible(was_visible).log_err();
                state.platform_surfaces.insert(surface, platform_surface);
            }
            drop(state);
            self.synchronize_platform_order().log_err();
            return Err(error);
        }
        let mut state = self.state.borrow_mut();
        state.visible.remove(&surface);
        state.input_handlers.remove(&surface);
        if state.pointer_capture == Some(surface) {
            state.pointer_capture = None;
            state.pressed_buttons.clear();
        }
        if state.hovered_native == Some(surface) {
            state.hovered_native = None;
        }
        Ok(())
    }

    /// Returns the renderer kind for a surface.
    pub fn surface_kind(&self, surface: CompositionSurfaceId) -> Result<CompositionSurfaceKind> {
        self.state.borrow().tree.kind(surface)
    }

    /// Returns a surface's parent.
    pub fn parent(&self, surface: CompositionSurfaceId) -> Result<Option<CompositionSurfaceId>> {
        self.state.borrow().tree.parent(surface)
    }

    /// Returns the ordered children of a surface, or the root surfaces when
    /// `parent` is `None`.
    pub fn children(
        &self,
        parent: Option<CompositionSurfaceId>,
    ) -> Result<Vec<CompositionSurfaceId>> {
        Ok(self.state.borrow().tree.children(parent)?.to_vec())
    }

    /// Applies `change` to the tree and the platform, or neither.
    fn change_tree(&self, change: impl FnOnce(&mut CompositionTree) -> Result<()>) -> Result<()> {
        let previous_tree = self.state.borrow().tree.clone();
        change(&mut self.state.borrow_mut().tree)?;
        if let Err(error) = self.synchronize_platform_order() {
            self.state.borrow_mut().tree = previous_tree;
            self.synchronize_platform_order().log_err();
            return Err(error);
        }
        Ok(())
    }

    fn register_platform_surface(
        &self,
        kind: CompositionSurfaceKind,
        platform_surface: Rc<dyn PlatformSurfaceAttachment>,
    ) -> Result<WindowCompositionSurface> {
        let platform_surface = Rc::new(ManagedPlatformSurface::new(
            platform_surface,
            &self.state.borrow(),
        ));
        let visible = platform_surface.visible.clone();
        let mut state = self.state.borrow_mut();
        let id = state.tree.insert(kind, None)?;
        let overlay_surface = state.overlay_surface;
        state.tree.place_below(id, overlay_surface)?;
        state.platform_surfaces.insert(id, platform_surface.clone());
        state.visible.insert(id, visible);
        drop(state);
        if let Err(error) = self.synchronize_platform_order() {
            platform_surface.set_visible(false).log_err();
            let mut state = self.state.borrow_mut();
            state.platform_surfaces.remove(&id);
            state.visible.remove(&id);
            state.tree.remove(id)?;
            drop(state);
            self.synchronize_platform_order().log_err();
            return Err(error);
        }
        Ok(surface_handle(id, Some(platform_surface)))
    }

    /// Hands the platform the tree's order and every surface's geometry.
    fn synchronize_platform_order(&self) -> Result<()> {
        let state = self.state.borrow();
        let mut origins = FxHashMap::default();
        let mut surfaces = Vec::new();
        for id in state.tree.flattened() {
            let parent = state.tree.parent(id)?;
            let parent_origin = parent
                .and_then(|parent| origins.get(&parent).copied())
                .unwrap_or_default();
            let content = match state.tree.kind(id)? {
                CompositionSurfaceKind::Gpui => PlatformCompositionSurfaceContent::Gpui,
                CompositionSurfaceKind::Native => PlatformCompositionSurfaceContent::Native(
                    state
                        .platform_surfaces
                        .get(&id)
                        .context("native composition surface is missing")?
                        .clone(),
                ),
                CompositionSurfaceKind::ExternalGpu => {
                    PlatformCompositionSurfaceContent::ExternalGpu(
                        state
                            .platform_surfaces
                            .get(&id)
                            .context("external GPU composition surface is missing")?
                            .clone(),
                    )
                }
            };
            let window_bounds = match &content {
                PlatformCompositionSurfaceContent::Gpui => None,
                PlatformCompositionSurfaceContent::Native(platform_surface)
                | PlatformCompositionSurfaceContent::ExternalGpu(platform_surface) => {
                    Some(platform_surface.bounds())
                }
            };
            let window_origin = window_bounds.map_or_else(Point::default, |bounds| bounds.origin);
            origins.insert(id, window_origin);
            surfaces.push(PlatformCompositionSurface {
                id,
                parent,
                window_origin,
                parent_origin,
                window_bounds,
                content,
            });
        }
        self.platform_window.set_composition_order(&surfaces)?;
        state.geometry_dirty.set(false);
        drop(state);
        self.state.borrow_mut().input_order = surfaces.iter().map(|surface| surface.id).collect();
        Ok(())
    }
}

fn surface_handle(
    id: CompositionSurfaceId,
    platform_surface: Option<Rc<dyn PlatformSurfaceAttachment>>,
) -> WindowCompositionSurface {
    WindowCompositionSurface(Rc::new(WindowCompositionSurfaceInner {
        id,
        platform_surface,
    }))
}

impl Window {
    /// Enables native window composition and returns its controller.
    ///
    /// GPUI's root scene is rendered below surfaces created by the controller,
    /// while deferred and window-level overlays are rendered above them.
    pub fn enable_window_composition(&self) -> Result<WindowComposition<'_>> {
        self.platform_window.enable_window_composition()?;
        let composition = WindowComposition {
            platform_window: self.platform_window.as_ref(),
            state: self.fast_composition.0.clone(),
        };
        composition.synchronize_platform_order()?;
        composition.state.borrow_mut().enabled = true;
        Ok(composition)
    }

    /// Prepaints or paints an element scope on a GPUI composition surface,
    /// then restores the previous surface. Wrap both phases when selecting a
    /// surface explicitly so its input regions follow its painted content.
    ///
    /// Inside a scroll layer, whose content is composited from tiles, the
    /// scope paints where the layer does.
    pub fn with_composition_surface<R>(
        &mut self,
        surface: CompositionSurfaceId,
        paint: impl FnOnce(&mut Self) -> R,
    ) -> Result<R> {
        self.invalidator.debug_assert_paint_or_prepaint();
        anyhow::ensure!(
            self.fast_composition.0.borrow().tree.kind(surface)? == CompositionSurfaceKind::Gpui,
            "paint operations can only target GPUI composition surfaces"
        );
        if self.fast_layers.painting.is_some() {
            return Ok(paint(self));
        }
        if self.invalidator.inner.borrow().draw_phase == crate::window::DrawPhase::Prepaint {
            let previous = self
                .next_frame
                .fast_composition_input
                .scope
                .replace(surface);
            let result = paint(self);
            self.next_frame.fast_composition_input.scope = previous;
            return Ok(result);
        }
        let previous_surface = self
            .next_frame
            .fast_composition_starts
            .current()
            .context("composition paint scope has no base surface")?;
        switch_surface(self, surface);
        let result = paint(self);
        switch_surface(self, previous_surface);
        Ok(result)
    }
}

fn switch_surface(window: &mut Window, surface: CompositionSurfaceId) {
    let scene_len = window.next_frame.scene.len();
    window
        .next_frame
        .fast_composition_starts
        .switch_to(surface, scene_len);
}

/// Stacks what the root view positions, as it prepaints, below the overlay
/// surface.
pub(crate) fn begin_base_prepaint(window: &mut Window) {
    begin_input(window, false);
}

/// Stacks what deferred and window-level draws position, as they prepaint,
/// above the overlay surface.
pub(crate) fn begin_overlay_prepaint(window: &mut Window) {
    begin_input(window, true);
}

/// Paints what follows, the root view, onto the base surface.
pub(crate) fn begin_base(window: &mut Window) {
    let state = window.fast_composition.0.borrow();
    state.prepainting.set(None);
    let surface = state.base_surface;
    drop(state);
    switch_surface(window, surface);
}

/// Moves each root native surface whose host changed surface to that
/// surface's side of the overlay surface.
fn restack_native_surfaces(state: &mut WindowCompositionState) -> Result<()> {
    let overlay = state.overlay_surface;
    for (id, surface) in &state.platform_surfaces {
        if !surface.restack.replace(false) || state.tree.parent(*id)?.is_some() {
            continue;
        }
        match surface.host.get() {
            Host::Base => state.tree.place_below(*id, overlay)?,
            Host::Overlay => state.tree.place_above(*id, overlay)?,
        }
    }
    Ok(())
}

/// Paints what follows, deferred and window-level draws, onto the overlay
/// surface.
pub(crate) fn begin_overlay(window: &mut Window) {
    let surface = window.fast_composition.0.borrow().overlay_surface;
    switch_surface(window, surface);
}

/// Carries the surface switches last frame made within `range` into the
/// frame being drawn, as [`Window::reuse_paint`] replays its scene, and
/// leaves painting on the surface it was on before.
pub(crate) fn reuse_starts(window: &mut Window, range: &Range<PaintIndex>) {
    let scenes = range.start.scene_index..range.end.scene_index;
    let starts = &window.rendered_frame.fast_composition_starts.0;
    let first = starts.partition_point(|(_, start)| *start < scenes.start);
    let last = starts.partition_point(|(_, start)| *start < scenes.end);
    if first == last {
        return;
    }
    let Some(surface) = window.next_frame.fast_composition_starts.current() else {
        return;
    };
    let offset = window.next_frame.scene.len();
    for index in first..last {
        let (reused, start) = window.rendered_frame.fast_composition_starts.0[index];
        window
            .next_frame
            .fast_composition_starts
            .switch_to(reused, start - scenes.start + offset);
    }
    let end = offset + scenes.len();
    window
        .next_frame
        .fast_composition_starts
        .switch_to(surface, end);
}

// Host changes must reach input ownership before the frame's hit test.
fn synchronize_geometry(window: &mut Window) {
    let state = window.fast_composition.0.clone();
    if !state.borrow().enabled {
        return;
    }
    if state.borrow().geometry_dirty.get() {
        if let Err(error) = restack_native_surfaces(&mut state.borrow_mut()) {
            log::error!("restacking native composition surfaces: {error:#}");
        }
        let composition = WindowComposition {
            platform_window: window.platform_window.as_ref(),
            state: state.clone(),
        };
        if let Err(error) = composition.synchronize_platform_order() {
            log::error!("updating window composition geometry: {error:#}");
        }
    }
}

/// Presents the frame last drawn: on its one surface, or split along the
/// composition tree once the window composes.
pub(crate) fn present(window: &mut Window) {
    let state = window.fast_composition.0.clone();
    if !state.borrow().enabled {
        window.platform_window.draw(&window.rendered_frame.scene);
        return;
    }
    synchronize_geometry(window);
    let state = state.borrow();
    let scene = &window.rendered_frame.scene;
    let layers = composed_scene_layers(
        &state.tree,
        &window.rendered_frame.fast_composition_starts.0,
        scene.len(),
    )
    .unwrap_or_else(|error| {
        log::error!("invalid window composition scene: {error:#}");
        vec![ComposedSceneLayer {
            surface: state.base_surface,
            ranges: vec![0..scene.len()],
        }]
    });
    drop(state);
    window.platform_window.draw_composed(ComposedScene::new(
        scene,
        layers,
        &window.rendered_frame.fast_composition_input,
    ));
}

/// Visibility and ancestor clips come from the existing managed surface tree.
fn input_bounds(
    state: &WindowCompositionState,
    surface: CompositionSurfaceId,
    mut bounds: Bounds<Pixels>,
    scale: f32,
) -> Option<Bounds<Pixels>> {
    let mut current = Some(surface);
    while let Some(id) = current {
        state.tree.kind(id).ok()?;
        if state.visible.get(&id).is_some_and(|visible| !visible.get()) {
            return None;
        }
        if let Some(native) = state.platform_surfaces.get(&id) {
            bounds = bounds.intersect(&native.bounds().to_pixels(scale));
        }
        current = state.tree.parent(id).ok()?;
    }
    (!bounds.is_empty()).then_some(bounds)
}

fn visible_surface(state: &WindowCompositionState, surface: CompositionSurfaceId) -> bool {
    let mut current = Some(surface);
    while let Some(id) = current {
        if state.tree.kind(id).is_err() || state.visible.get(&id).is_some_and(|v| !v.get()) {
            return false;
        }
        current = state.tree.parent(id).ok().flatten();
    }
    true
}

fn native_target(window: &Window, scroll: bool) -> Option<CompositionSurfaceId> {
    let state = window.fast_composition.0.borrow();
    if !state.enabled || state.platform_surfaces.is_empty() {
        return None;
    }
    if !scroll {
        if let Some(captured) = state
            .pointer_capture
            .filter(|id| visible_surface(&state, *id))
        {
            return Some(captured);
        }
    }
    let point = window.mouse_position;
    for surface in state.input_order.iter().rev().copied() {
        if !visible_surface(&state, surface) {
            continue;
        }
        if let Some(native) = state.platform_surfaces.get(&surface) {
            if input_bounds(
                &state,
                surface,
                native.bounds().to_pixels(window.scale_factor()),
                window.scale_factor(),
            )
            .is_some_and(|bounds| bounds.contains(&point))
            {
                return Some(surface);
            }
        } else if let Some(regions) = window
            .rendered_frame
            .fast_composition_input
            .hitboxes
            .get(&surface)
        {
            if regions.iter().rev().any(|hitbox| {
                hitbox.bounds.contains(&point)
                    && (!scroll || hitbox.behavior == HitboxBehavior::BlockMouse)
            }) {
                return None;
            }
        }
    }
    None
}

/// OS-directed native events still run GPUI capture, but cannot activate
/// unrelated GPUI widgets underneath the native view.
fn native_mouse(window: &Window, event: &dyn Any) -> bool {
    if window.captured_hitbox.is_some() {
        let state = window.fast_composition.0.borrow();
        return state
            .pointer_capture
            .is_some_and(|id| visible_surface(&state, id));
    }
    if event
        .downcast_ref::<MouseMoveEvent>()
        .is_some_and(|event| event.pressed_button.is_some())
        && window.fast_composition.0.borrow().pointer_capture.is_none()
    {
        return false;
    }
    native_target(window, false).is_some()
}

pub(crate) fn bubble_mouse(window: &Window, event: &dyn Any, cx: &crate::App) -> bool {
    cx.propagate_event && (cx.has_active_drag() || !native_mouse(window, event))
}

pub(crate) fn event_target(window: &Window, event: &PlatformInput) -> Option<CompositionSurfaceId> {
    if !matches!(
        event,
        PlatformInput::MouseDown(_)
            | PlatformInput::MouseUp(_)
            | PlatformInput::MouseMove(_)
            | PlatformInput::ScrollWheel(_)
    ) {
        return None;
    }
    let scroll = matches!(event, PlatformInput::ScrollWheel(_));
    if !scroll {
        let state = window.fast_composition.0.borrow();
        if let Some(captured) = state.pointer_capture {
            return visible_surface(&state, captured).then_some(captured);
        }
    }
    if window.captured_hitbox.is_some() || matches!(event, PlatformInput::MouseUp(_)) {
        return None;
    }
    if matches!(
        event,
        PlatformInput::MouseMove(MouseMoveEvent {
            pressed_button: Some(_),
            ..
        })
    ) && window.fast_composition.0.borrow().pointer_capture.is_none()
    {
        return None;
    }
    native_target(window, scroll)
}

/// Called after the existing dispatcher; windowless producers get one delivery,
/// while OS-managed views continue through the platform's original event path.
pub(crate) fn dispatch_native(
    window: &mut Window,
    event: &PlatformInput,
    target: Option<CompositionSurfaceId>,
    propagate: &mut bool,
) {
    let pointer = matches!(
        event,
        PlatformInput::MouseDown(_)
            | PlatformInput::MouseUp(_)
            | PlatformInput::MouseMove(_)
            | PlatformInput::MouseExited(_)
            | PlatformInput::ScrollWheel(_)
    );
    if !pointer || !window.fast_composition.0.borrow().enabled {
        return;
    }
    let target = target.filter(|id| visible_surface(&window.fast_composition.0.borrow(), *id));
    let (handler, leave) = {
        let mut state = window.fast_composition.0.borrow_mut();
        let leave = if matches!(
            event,
            PlatformInput::MouseMove(_) | PlatformInput::MouseExited(_)
        ) {
            let current = if matches!(event, PlatformInput::MouseExited(_)) {
                None
            } else {
                target
            };
            let previous = std::mem::replace(&mut state.hovered_native, current);
            previous
                .filter(|old| Some(*old) != current)
                .and_then(|id| state.input_handlers.get(&id).cloned())
        } else {
            None
        };
        if *propagate && !window.default_prevented {
            if let PlatformInput::MouseDown(down) = event {
                if let Some(id) = target {
                    state.pointer_capture = Some(id);
                    if !state.pressed_buttons.contains(&down.button) {
                        state.pressed_buttons.push(down.button);
                    }
                }
            }
        }
        let owned_release = matches!(event, PlatformInput::MouseUp(_))
            && target.is_some()
            && state.pointer_capture == target;
        let handler = if (*propagate && !window.default_prevented) || owned_release {
            target.and_then(|id| state.input_handlers.get(&id).cloned())
        } else {
            None
        };
        if let PlatformInput::MouseUp(up) = event {
            // A single press ends even if platform normalization changed its button
            // (AppKit ctrl-click with Control released before mouse-up).
            if state.pressed_buttons.len() == 1 {
                state.pressed_buttons.clear();
            } else {
                state.pressed_buttons.retain(|button| *button != up.button);
            }
            if state.pressed_buttons.is_empty() {
                state.pointer_capture = None;
            }
        }
        (handler, leave)
    };
    if let Some(leave) = leave {
        let (position, pressed_button, modifiers) = match event {
            PlatformInput::MouseMove(event) => {
                (event.position, event.pressed_button, event.modifiers)
            }
            PlatformInput::MouseExited(event) => {
                (event.position, event.pressed_button, event.modifiers)
            }
            _ => unreachable!(),
        };
        leave.borrow_mut()(&PlatformInput::MouseExited(MouseExitEvent {
            position,
            pressed_button,
            modifiers,
        }));
    }
    if let Some(handler) = handler {
        let result = handler.borrow_mut()(event);
        *propagate &= result.propagate;
        window.default_prevented |= result.default_prevented;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CompositionSurfaceId, CompositionSurfaceKind, CompositionTree, Host,
        ManagedPlatformSurface, PlatformSurfaceAttachment, composed_scene_layers,
        restack_native_surfaces,
    };
    use crate::{
        AppContext as _, Bounds, ContentMask, Context, DevicePixels, Entity, IntoElement,
        ParentElement as _, Point, Quad, Render, ScaledPixels, Scene, Size, StyleRefinement,
        Styled as _, TestAppContext, Window, canvas, deferred, div, fill, px,
        scene::PaintOperation, size,
    };
    use anyhow::Result;
    use std::{any::Any, cell::Cell, rc::Rc};

    #[test]
    fn composition_surfaces_have_stable_identity_and_explicit_order() -> Result<()> {
        let mut tree = CompositionTree::new();
        let base = tree.insert(CompositionSurfaceKind::Gpui, None)?;
        let webview = tree.insert(CompositionSurfaceKind::Native, None)?;
        let overlay = tree.insert(CompositionSurfaceKind::Gpui, None)?;

        tree.place_below(overlay, webview)?;

        assert_eq!(tree.flattened(), [base, overlay, webview]);
        assert_eq!(tree.kind(webview)?, CompositionSurfaceKind::Native);
        Ok(())
    }

    #[test]
    fn composition_surfaces_can_be_nested_and_reparented() -> Result<()> {
        let mut tree = CompositionTree::new();
        let base = tree.insert(CompositionSurfaceKind::Gpui, None)?;
        let pane = tree.insert(CompositionSurfaceKind::Native, Some(base))?;
        let video = tree.insert(CompositionSurfaceKind::ExternalGpu, None)?;

        tree.reparent(video, Some(pane))?;

        assert_eq!(tree.parent(video)?, Some(pane));
        assert_eq!(tree.children(Some(pane))?, [video]);
        assert_eq!(tree.flattened(), [base, pane, video]);
        Ok(())
    }

    #[test]
    fn composition_tree_rejects_cycles_and_cross_parent_ordering() -> Result<()> {
        let mut tree = CompositionTree::new();
        let parent = tree.insert(CompositionSurfaceKind::Gpui, None)?;
        let child = tree.insert(CompositionSurfaceKind::Native, Some(parent))?;
        let sibling = tree.insert(CompositionSurfaceKind::Gpui, None)?;

        assert!(tree.reparent(parent, Some(child)).is_err());
        assert!(tree.place_above(child, sibling).is_err());
        assert_eq!(tree.parent(parent)?, None);
        assert_eq!(tree.parent(child)?, Some(parent));
        Ok(())
    }

    #[test]
    fn removing_a_surface_preserves_and_reparents_its_children() -> Result<()> {
        let mut tree = CompositionTree::new();
        let parent = tree.insert(CompositionSurfaceKind::Gpui, None)?;
        let child = tree.insert(CompositionSurfaceKind::Native, Some(parent))?;

        tree.remove(parent)?;

        assert_eq!(tree.parent(child)?, None);
        assert_eq!(tree.flattened(), [child]);
        Ok(())
    }

    #[test]
    fn scene_ranges_follow_tree_order_and_preserve_repeated_segments() -> Result<()> {
        let mut tree = CompositionTree::new();
        let base = tree.insert(CompositionSurfaceKind::Gpui, None)?;
        let native = tree.insert(CompositionSurfaceKind::Native, None)?;
        let overlay = tree.insert(CompositionSurfaceKind::Gpui, None)?;
        let starts = [(base, 0), (overlay, 4), (base, 7)];

        let layers = composed_scene_layers(&tree, &starts, 10)?;

        let [base_layer, overlay_layer] = layers.as_slice() else {
            panic!("expected base and overlay scene layers");
        };
        assert_eq!(base_layer.surface, base);
        assert_eq!(base_layer.ranges, [0..4, 7..10]);
        assert_eq!(overlay_layer.surface, overlay);
        assert_eq!(overlay_layer.ranges, [4..7]);
        assert_eq!(tree.kind(native)?, CompositionSurfaceKind::Native);
        Ok(())
    }

    #[test]
    fn scene_ranges_reject_non_gpui_targets_and_non_monotonic_markers() -> Result<()> {
        let mut tree = CompositionTree::new();
        let base = tree.insert(CompositionSurfaceKind::Gpui, None)?;
        let native = tree.insert(CompositionSurfaceKind::Native, None)?;

        assert!(composed_scene_layers(&tree, &[(native, 0)], 1).is_err());
        assert!(composed_scene_layers(&tree, &[(base, 2), (base, 1)], 2).is_err());
        Ok(())
    }

    struct TestPlatformSurface {
        bounds_writes: Cell<usize>,
        fail_next_bounds: Cell<bool>,
        bounds: Cell<Bounds<DevicePixels>>,
        parent_origin: Cell<Point<DevicePixels>>,
    }

    impl PlatformSurfaceAttachment for TestPlatformSurface {
        fn set_bounds(&self, bounds: Bounds<DevicePixels>) -> Result<()> {
            self.bounds_writes.set(self.bounds_writes.get() + 1);
            if self.fail_next_bounds.replace(false) {
                return Err(anyhow::anyhow!("test bounds failure"));
            }
            self.bounds.set(bounds);
            Ok(())
        }

        fn bounds(&self) -> Bounds<DevicePixels> {
            self.bounds.get()
        }

        fn set_parent_origin(&self, origin: Point<DevicePixels>) -> Result<()> {
            self.parent_origin.set(origin);
            Ok(())
        }

        fn set_visible(&self, _visible: bool) -> Result<()> {
            Ok(())
        }

        fn platform_handle(&self) -> Result<Box<dyn Any>> {
            Ok(Box::new(()))
        }
    }

    #[test]
    fn managed_composition_surface_tracks_window_geometry_changes() {
        let platform_surface = Rc::new(TestPlatformSurface {
            bounds_writes: Cell::new(0),
            fail_next_bounds: Cell::new(false),
            bounds: Cell::new(Bounds::default()),
            parent_origin: Cell::new(Point::default()),
        });
        let geometry_dirty = Rc::new(Cell::new(false));
        let surface = ManagedPlatformSurface {
            platform_surface: platform_surface.clone(),
            bounds: Cell::new(Bounds::default()),
            geometry_dirty: geometry_dirty.clone(),
            visible: Rc::new(Cell::new(true)),
            input_forwarding: Cell::new(false),
            prepainting: Rc::new(Cell::new(None)),
            host: Cell::new(Host::Base),
            restack: Cell::new(false),
        };
        let bounds = Bounds {
            origin: Point {
                x: DevicePixels(25),
                y: DevicePixels(40),
            },
            size: size(DevicePixels(300), DevicePixels(200)),
        };
        let parent_origin = Point {
            x: DevicePixels(10),
            y: DevicePixels(15),
        };

        assert!(surface.set_bounds(bounds).is_ok());
        assert!(surface.set_parent_origin(parent_origin).is_ok());

        assert_eq!(surface.bounds(), bounds);
        assert_eq!(platform_surface.bounds.get(), bounds);
        assert_eq!(platform_surface.parent_origin.get(), parent_origin);
        assert!(geometry_dirty.get());
    }

    #[test]
    fn unchanged_native_bounds_do_not_resynchronize_composition() -> Result<()> {
        let bounds = Bounds::new(Point::default(), size(DevicePixels(300), DevicePixels(200)));
        let platform = Rc::new(TestPlatformSurface {
            bounds_writes: Cell::new(0),
            fail_next_bounds: Cell::new(false),
            bounds: Cell::new(Bounds::default()),
            parent_origin: Cell::new(Point::default()),
        });
        let dirty = Rc::new(Cell::new(false));
        let surface = ManagedPlatformSurface {
            platform_surface: platform.clone(),
            bounds: Cell::new(Bounds::default()),
            geometry_dirty: dirty.clone(),
            visible: Rc::new(Cell::new(true)),
            input_forwarding: Cell::new(false),
            prepainting: Rc::new(Cell::new(None)),
            host: Cell::new(Host::Base),
            restack: Cell::new(false),
        };
        surface.set_bounds(bounds)?;
        dirty.set(false);
        for _ in 0..10 {
            surface.set_bounds(bounds)?;
        }
        assert!(
            !dirty.get(),
            "unchanged layout must not remount native views between mouse down and up"
        );
        assert_eq!(platform.bounds_writes.get(), 1);
        let moved = Bounds::new(
            Point {
                x: DevicePixels(20),
                y: DevicePixels(15),
            },
            bounds.size,
        );
        surface.set_bounds(moved)?;
        assert!(dirty.get());
        assert_eq!(platform.bounds_writes.get(), 2);
        assert_eq!(surface.bounds(), moved);
        Ok(())
    }

    #[test]
    fn failed_native_bounds_updates_remain_retryable() -> Result<()> {
        let bounds = Bounds::new(Point::default(), size(DevicePixels(600), DevicePixels(400)));
        let platform = Rc::new(TestPlatformSurface {
            bounds_writes: Cell::new(0),
            fail_next_bounds: Cell::new(true),
            bounds: Cell::new(Bounds::default()),
            parent_origin: Cell::new(Point::default()),
        });
        let dirty = Rc::new(Cell::new(false));
        let surface = ManagedPlatformSurface {
            platform_surface: platform.clone(),
            bounds: Cell::new(Bounds::default()),
            geometry_dirty: dirty.clone(),
            visible: Rc::new(Cell::new(true)),
            input_forwarding: Cell::new(false),
            prepainting: Rc::new(Cell::new(None)),
            host: Cell::new(Host::Base),
            restack: Cell::new(false),
        };
        assert!(surface.set_bounds(bounds).is_err());
        assert_eq!(surface.bounds(), Bounds::default());
        assert!(!dirty.get());
        surface.set_bounds(bounds)?;
        assert!(dirty.get());
        assert_eq!(surface.bounds(), bounds);
        assert_eq!(platform.bounds_writes.get(), 2);
        dirty.set(false);
        surface.set_bounds(bounds)?;
        assert!(!dirty.get());
        assert_eq!(platform.bounds_writes.get(), 2);
        Ok(())
    }

    fn square(side: f32) -> Bounds<ScaledPixels> {
        Bounds {
            origin: Point::default(),
            size: Size {
                width: ScaledPixels::from(side),
                height: ScaledPixels::from(side),
            },
        }
    }

    #[test]
    fn empty_layers_do_not_make_a_scene_drawable() {
        let mut scene = Scene::default();
        scene.push_layer(square(100.));
        scene.pop_layer();

        assert_ne!(scene.len(), 0);
        assert!(scene.is_empty());
    }

    #[test]
    fn drawable_primitives_make_a_scene_non_empty() {
        let mut scene = Scene::default();
        let bounds = square(100.);
        scene.insert_primitive(Quad {
            bounds,
            content_mask: ContentMask { bounds },
            ..Default::default()
        });

        assert!(!scene.is_empty());
    }

    #[test]
    fn replay_preserves_scene_emptiness() {
        let mut source = Scene::default();
        source.push_layer(square(100.));
        source.pop_layer();

        let mut replayed = Scene::default();
        replayed.replay(0..source.len(), &source);

        assert!(replayed.is_empty());
    }

    #[test]
    fn balanced_replay_restores_layers_crossing_the_range_boundary() {
        let mut source = Scene::default();
        let inner_bounds = square(50.);
        source.push_layer(square(100.));
        source.push_layer(inner_bounds);
        source.insert_primitive(Quad {
            bounds: inner_bounds,
            content_mask: ContentMask {
                bounds: inner_bounds,
            },
            ..Default::default()
        });
        source.pop_layer();
        source.pop_layer();

        let mut replayed = Scene::default();
        assert!(replayed.replay_balanced(2..3, &source).is_ok());

        assert_eq!(replayed.paint_operations.len(), 5);
        assert!(matches!(
            replayed.paint_operations.as_slice(),
            [
                PaintOperation::StartLayer(_),
                PaintOperation::StartLayer(_),
                PaintOperation::Primitive(_),
                PaintOperation::EndLayer,
                PaintOperation::EndLayer
            ]
        ));
    }

    #[test]
    fn balanced_replay_rejects_invalid_ranges() {
        let source = Scene::default();
        let mut replayed = Scene::default();
        let start = 1;
        let end = 0;

        assert!(replayed.replay_balanced(start..end, &source).is_err());
        assert!(replayed.replay_balanced(0..1, &source).is_err());
    }

    struct Badge {
        overlay: CompositionSurfaceId,
        builds: Rc<Cell<usize>>,
    }

    impl Render for Badge {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            self.builds.set(self.builds.get() + 1);
            let overlay = self.overlay;
            div()
                .size_full()
                .bg(crate::black())
                .child(
                    canvas(
                        |_, _, _| (),
                        move |bounds, _, window, _| {
                            window
                                .with_composition_surface(overlay, |window| {
                                    window.paint_quad(fill(bounds, crate::red()))
                                })
                                .unwrap();
                        },
                    )
                    .size(px(20.)),
                )
                .child(div().size(px(10.)).bg(crate::white()))
        }
    }

    struct Panel {
        badge: Entity<Badge>,
    }

    impl Render for Panel {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size(px(300.))
                .bg(crate::white())
                .child(
                    self.badge
                        .clone()
                        .cached(StyleRefinement::default().w(px(100.)).h(px(20.))),
                )
                .child(div().size(px(30.)).bg(crate::black()))
        }
    }

    fn surface_of_each_operation(window: &Window) -> Vec<CompositionSurfaceId> {
        let state = window.fast_composition.0.borrow();
        let frame = &window.rendered_frame;
        let layers = composed_scene_layers(
            &state.tree,
            &frame.fast_composition_starts.0,
            frame.scene.len(),
        )
        .unwrap();
        let mut surfaces = vec![None; frame.scene.len()];
        for layer in layers {
            for range in layer.ranges {
                for surface in &mut surfaces[range] {
                    *surface = Some(layer.surface);
                }
            }
        }
        surfaces.into_iter().map(Option::unwrap).collect()
    }

    /// A retained view that painted onto the overlay surface still does when
    /// it is drawn again from last frame: its surface switches come along
    /// with its scene, and painting returns to the base surface after it.
    #[crate::test]
    fn reused_views_keep_their_composition_surfaces(cx: &mut TestAppContext) {
        let builds = Rc::new(Cell::new(0));
        let window = cx.add_window({
            let builds = builds.clone();
            move |window, cx| {
                let overlay = window.fast_composition.0.borrow().overlay_surface;
                Panel {
                    badge: cx.new(|_| Badge { overlay, builds }),
                }
            }
        });
        let draw = |cx: &mut TestAppContext| {
            cx.update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
                let state = window.fast_composition.0.borrow();
                (
                    surface_of_each_operation(window),
                    state.base_surface,
                    state.overlay_surface,
                )
            })
            .unwrap()
        };

        let (first, base, overlay) = draw(cx);
        assert_eq!(builds.get(), 1);
        assert_eq!(first.iter().filter(|s| **s == overlay).count(), 1);
        assert_eq!(first.last(), Some(&base));

        window.update(cx, |_, _, cx| cx.notify()).unwrap();
        let (second, _, _) = draw(cx);
        assert_eq!(builds.get(), 1, "the badge is drawn again from last frame");
        assert_eq!(second, first);
    }

    fn test_platform_surface() -> Rc<TestPlatformSurface> {
        Rc::new(TestPlatformSurface {
            bounds_writes: Cell::new(0),
            fail_next_bounds: Cell::new(false),
            bounds: Cell::new(Bounds::default()),
            parent_origin: Cell::new(Point::default()),
        })
    }

    /// Positions one native surface from the root view and another from a
    /// deferred draw, or both from the root view.
    struct Hosts {
        page: Rc<ManagedPlatformSurface>,
        dialog_page: Rc<ManagedPlatformSurface>,
        dialog_open: bool,
    }

    fn native_host(surface: Rc<ManagedPlatformSurface>) -> impl IntoElement {
        canvas(
            move |bounds, window, _| {
                surface
                    .set_bounds(bounds.to_device_pixels(window.scale_factor()))
                    .unwrap()
            },
            |_, _, _, _| {},
        )
        .size(px(50.))
    }

    impl Render for Hosts {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let dialog_page = native_host(self.dialog_page.clone());
            let root = div().size_full().child(native_host(self.page.clone()));
            if self.dialog_open {
                root.child(deferred(
                    div().size(px(80.)).bg(crate::white()).child(dialog_page),
                ))
            } else {
                root.child(dialog_page)
            }
        }
    }

    /// A native surface hosted in a deferred draw, a web page in a dialog,
    /// stacks above the overlay surface the dialog's background is drawn on,
    /// and returns below it once hosted by the root view again.
    #[crate::test]
    fn native_surfaces_stack_with_their_hosts(cx: &mut TestAppContext) {
        let names = Rc::new(std::cell::RefCell::new(Vec::new()));
        let window = cx.add_window({
            let names = names.clone();
            move |window, _| {
                let mut state = window.fast_composition.0.borrow_mut();
                let overlay = state.overlay_surface;
                let mut names = names.borrow_mut();
                names.push((state.base_surface, "base"));
                names.push((overlay, "overlay"));
                let mut native = |name| {
                    let id = state
                        .tree
                        .insert(CompositionSurfaceKind::Native, None)
                        .unwrap();
                    state.tree.place_below(id, overlay).unwrap();
                    let surface =
                        Rc::new(ManagedPlatformSurface::new(test_platform_surface(), &state));
                    state.platform_surfaces.insert(id, surface.clone());
                    names.push((id, name));
                    surface
                };
                Hosts {
                    page: native("page"),
                    dialog_page: native("dialog page"),
                    dialog_open: true,
                }
            }
        });
        let draw = |cx: &mut TestAppContext| {
            cx.update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
                let mut state = window.fast_composition.0.borrow_mut();
                restack_native_surfaces(&mut state).unwrap();
                let names = names.borrow();
                state
                    .tree
                    .flattened()
                    .into_iter()
                    .map(|id| names.iter().find(|(named, _)| *named == id).unwrap().1)
                    .collect::<Vec<_>>()
            })
            .unwrap()
        };

        assert_eq!(draw(cx), ["base", "page", "overlay", "dialog page"]);

        window
            .update(cx, |hosts, _, cx| {
                hosts.dialog_open = false;
                cx.notify();
            })
            .unwrap();
        assert_eq!(draw(cx), ["base", "page", "dialog page", "overlay"]);
    }
}
