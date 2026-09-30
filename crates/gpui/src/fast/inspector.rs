//! `App::register_inspector_element` in the form newer upstream GPUI has it.
//!
//! Upstream moved from registering a renderer to registering a factory, which
//! each window's inspector calls the first time it renders that state, so a
//! renderer can own an entity created for its window. GPUI Kit is written
//! against that form, and gpui-fast has to compile it: the factory is adapted
//! onto this snapshot's registry of renderers, making a renderer per window on
//! first use as upstream does.

use crate::{
    AnyWindowHandle, App, InspectorElementId, InspectorElementRegistry, IntoElement, Window,
};
use collections::FxHashMap;
use std::cell::RefCell;

pub(crate) fn register_element<T: 'static, R: IntoElement, F>(
    registry: &mut InspectorElementRegistry,
    factory: impl 'static + Fn(&mut Window, &mut App) -> F,
) where
    F: 'static + FnMut(InspectorElementId, &T, &mut Window, &mut App) -> R,
{
    registry.register(renderer_per_window(factory));
}

/// One renderer: calls `factory` for a window the first time that window
/// renders, and that window's renderer from then on.
pub(crate) fn renderer_per_window<T: 'static, R, F>(
    factory: impl 'static + Fn(&mut Window, &mut App) -> F,
) -> impl 'static + Fn(InspectorElementId, &T, &mut Window, &mut App) -> R
where
    F: 'static + FnMut(InspectorElementId, &T, &mut Window, &mut App) -> R,
{
    let renderers = RefCell::new(FxHashMap::<AnyWindowHandle, F>::default());
    move |id, state, window, cx| {
        let handle = window.window_handle();
        // Out of the map while it renders, so a renderer that reaches the
        // inspector again does not find the map borrowed.
        let taken = renderers.borrow_mut().remove(&handle);
        let mut renderer = taken.unwrap_or_else(|| factory(window, cx));
        let rendered = renderer(id, state, window, cx);
        renderers.borrow_mut().insert(handle, renderer);
        rendered
    }
}
