use crate::{
    AnyElement, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, Window,
};

/// Gives an element a key among its siblings, without a box of its own. Made
/// with [`IntoElement::key`].
///
/// Layout nodes are carried from one frame to the next by an element's path
/// from the root, and each step of that path is either the element's
/// [`ElementId`] or, when it has none, its position among the siblings that
/// have none either. Only the element placed directly among the siblings takes
/// that step. A component built with [`RenderOnce`](crate::RenderOnce)
/// reports no id of its own, whatever the element it renders into has, so a
/// list of components is matched by position: when a row is inserted at the
/// top, every row below it is somewhere else in the path, and every one of
/// them has its layout rebuilt.
///
/// A key is that step. It adds nothing to the layout: the element it wraps is
/// laid out, placed and painted exactly as it would be without it. Like an
/// [`ElementId`], it also scopes the element state of everything inside it, so
/// state follows the row rather than the position it happens to be drawn at.
pub struct Keyed {
    key: ElementId,
    child: AnyElement,
}

impl Keyed {
    pub(crate) fn new(key: ElementId, child: AnyElement) -> Self {
        Keyed { key, child }
    }
}

impl IntoElement for Keyed {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Keyed {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.key.clone())
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        // The child's node is this element's node: there is no box here to lay
        // out, only a step in the path the child's node is found by.
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
    }
}
