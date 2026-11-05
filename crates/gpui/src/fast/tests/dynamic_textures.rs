//! Tests of `fast::dynamic_textures` against a real window: what an update
//! does to retained views that would otherwise draw nothing again. The
//! payload and region validation tests live in the module itself.

use crate::{
    App, AppContext as _, Bounds, Context, DevicePixels, DynamicTexture, Element, ElementId,
    Entity, GlobalElementId, InspectorElementId, IntoElement, LayoutId, ParentElement as _, Render,
    Size, StyleRefinement, Styled as _, TestAppContext, Window, WindowHandle, div, px, size,
};
use std::{cell::Cell, rc::Rc};

/// A best-effort backend: the platform slot is process-wide and first wins,
/// so another test's mock may already be installed; any backend makes
/// create and update succeed, and this test never inspects its calls.
struct AnyBackend;

impl crate::DynamicTexturePlatform for AnyBackend {
    fn create_texture(
        &self,
        _: crate::DynamicTextureId,
        _: Size<DevicePixels>,
        _: &[u8],
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn update_texture(
        &self,
        _: crate::DynamicTextureId,
        _: Bounds<DevicePixels>,
        _: &[u8],
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn remove_texture(&self, _: crate::DynamicTextureId) {}
}

/// A view that paints a dynamic texture and counts its renders.
struct Stream {
    texture: Option<DynamicTexture>,
    paints: Rc<Cell<usize>>,
    created: Rc<Cell<Option<DynamicTexture>>>,
}

impl Render for Stream {
    fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.paints.set(self.paints.get() + 1);
        if self.texture.is_none() {
            let texture = window
                .create_dynamic_texture(size(DevicePixels(4), DevicePixels(4)), &[0u8; 64], 16)
                .expect("the backend is installed");
            self.created.set(Some(texture));
            self.texture = Some(texture);
        }
        TexturePaint {
            texture: self.texture.expect("created above"),
        }
    }
}

/// Paints the texture during the paint phase, as an app's custom element
/// would: [`crate::Window::paint_dynamic_texture`] asserts the paint phase.
struct TexturePaint {
    texture: DynamicTexture,
}

impl IntoElement for TexturePaint {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for TexturePaint {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (window.request_layout(crate::Style::default(), None, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<crate::Pixels>,
        _: &mut (),
        _: &mut Window,
        _: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<crate::Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        _: &mut App,
    ) {
        let _ = window.paint_dynamic_texture(
            Bounds {
                origin: crate::point(px(0.), px(0.)),
                size: size(px(64.), px(64.)),
            },
            self.texture,
        );
    }
}

/// A parent that caches the stream, so a frame where nothing the stream read
/// changed draws it again from last frame.
struct Shell {
    stream: Entity<Stream>,
}

impl Render for Shell {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size(px(300.))
            .child(self.stream.clone().cached(StyleRefinement::default()))
    }
}

fn window(
    cx: &mut TestAppContext,
) -> (
    WindowHandle<Shell>,
    Rc<Cell<usize>>,
    Rc<Cell<Option<DynamicTexture>>>,
) {
    let paints = Rc::new(Cell::new(0));
    let created: Rc<Cell<Option<DynamicTexture>>> = Rc::new(Cell::new(None));
    crate::set_dynamic_texture_platform(std::sync::Arc::new(AnyBackend));
    let window = cx.add_window(|_, cx| {
        let paints = paints.clone();
        let created = created.clone();
        Shell {
            stream: cx.new(|_| Stream {
                texture: None,
                paints,
                created,
            }),
        }
    });
    (window, paints, created)
}

/// An update, or a recorded GPU write, marks the window for a full redraw, so
/// a cached view showing the texture renders again — and carries the new
/// contents — even though nothing it read changed and it would otherwise draw
/// nothing again.
#[test]
fn a_write_renders_a_cached_view_showing_the_texture_again() {
    let mut cx = TestAppContext::single();
    let (window, paints, created) = window(&mut cx);

    cx.update_window(window.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
    })
    .unwrap();
    assert_eq!(paints.get(), 1, "the first draw paints the view");
    let texture = created.get().expect("the first draw created the texture");

    // Update the texture from outside the view's paint, without notifying
    // the view's entity: its cached subtree would otherwise be reused.
    cx.update_window(window.into(), |_, window, _| {
        window
            .update_dynamic_texture(
                texture,
                Bounds {
                    origin: crate::point(DevicePixels(0), DevicePixels(0)),
                    size: size(DevicePixels(2), DevicePixels(2)),
                },
                &[0u8; 16],
                8,
            )
            .expect("the update succeeds");
    })
    .unwrap();

    cx.update_window(window.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
    })
    .unwrap();
    assert_eq!(
        paints.get(),
        2,
        "the cached view must be rendered again after an update"
    );

    // A GPU producer's write is recorded the same way.
    cx.update_window(window.into(), |_, window, _| {
        window
            .refresh_dynamic_texture(texture)
            .expect("the texture is live");
    })
    .unwrap();
    cx.update_window(window.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
    })
    .unwrap();
    assert_eq!(
        paints.get(),
        3,
        "the cached view must be rendered again after a GPU write"
    );
}
