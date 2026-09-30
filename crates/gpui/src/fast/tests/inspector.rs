//! `App::register_inspector_element`'s factory, as `fast::inspector` adapts it.

use crate::fast::inspector::renderer_per_window;
use crate::{
    AnyWindowHandle, App, AppContext, Context, Empty, GlobalElementId, InspectorElementId,
    InspectorElementPath, IntoElement, Render, TestAppContext, Window,
};
use std::{cell::Cell, panic::Location, rc::Rc};

struct Blank;

impl Render for Blank {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

fn element_id() -> InspectorElementId {
    InspectorElementId {
        path: Rc::new(InspectorElementPath {
            global_id: GlobalElementId::default(),
            source_location: Location::caller(),
        }),
        instance_id: 0,
    }
}

#[test]
fn a_window_makes_its_renderer_once_and_keeps_it() {
    let mut cx = TestAppContext::single();
    let first: AnyWindowHandle = cx.add_window(|_, _| Blank).into();
    let second: AnyWindowHandle = cx.add_window(|_, _| Blank).into();

    let made = Rc::new(Cell::new(0));
    let render = Rc::new(renderer_per_window({
        let made = made.clone();
        move |_: &mut Window, _: &mut App| {
            made.set(made.get() + 1);
            // Counts its own calls, so a renderer made again starts over.
            let mut calls = 0;
            move |_: InspectorElementId, state: &usize, _: &mut Window, _: &mut App| {
                calls += 1;
                calls * 10 + *state
            }
        }
    }));
    let mut draw = |window: AnyWindowHandle, state: usize| {
        let render = render.clone();
        cx.update_window(window, move |_, window, cx| {
            render(element_id(), &state, window, cx)
        })
        .unwrap()
    };

    assert_eq!(draw(first, 1), 11);
    assert_eq!(draw(first, 2), 22, "the first window keeps its renderer");
    assert_eq!(made.get(), 1);
    assert_eq!(
        draw(second, 3),
        13,
        "a second window gets a renderer of its own"
    );
    assert_eq!(made.get(), 2);
    assert_eq!(draw(first, 4), 34);
    assert_eq!(made.get(), 2);
}
