//! Tests of [`crate::fast::misc::mouse_up_modality`].

use std::path::PathBuf;

use crate::{
    AppContext as _, Context, ExternalPaths, FileDropEvent, InteractiveElement as _, IntoElement,
    Modifiers, MouseButton, ParentElement as _, Render, StatefulInteractiveElement as _,
    Styled as _, TestAppContext, Window, div, point, px,
};

struct Dragged;

struct DraggedView;

impl Render for DraggedView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// A source to drag from on the left, and on the right a target that counts
/// the mouse ups, clicks and drops it gets.
#[derive(Default)]
struct Page {
    mouse_ups: usize,
    clicks: usize,
    drops: usize,
    file_drops: usize,
}

impl Render for Page {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let source = div()
            .id("source")
            .w(px(100.))
            .h(px(100.))
            .on_drag(Dragged, |_, _, _, cx| cx.new(|_| DraggedView));
        let target = div()
            .id("target")
            .w(px(100.))
            .h(px(100.))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.mouse_ups += 1),
            )
            .on_click(cx.listener(|this, _, _, _| this.clicks += 1))
            .on_drop(cx.listener(|this, _: &Dragged, _, _| this.drops += 1))
            .on_drop(cx.listener(|this, _: &ExternalPaths, _, _| this.file_drops += 1));
        div().flex().size_full().child(source).child(target)
    }
}

const SOURCE: (f32, f32) = (50., 50.);
const TARGET: (f32, f32) = (150., 50.);

#[crate::test]
fn a_release_after_a_key_press_reaches_its_listeners(cx: &mut TestAppContext) {
    let (page, cx) = cx.add_window_view(|_, _| Page::default());
    let target = point(px(TARGET.0), px(TARGET.1));
    cx.simulate_mouse_down(target, MouseButton::Left, Modifiers::none());
    cx.simulate_keystrokes("a");
    cx.simulate_mouse_up(target, MouseButton::Left, Modifiers::none());
    page.read_with(cx, |page, _| {
        assert_eq!(page.mouse_ups, 1);
        assert_eq!(page.clicks, 1);
    });
}

#[crate::test]
fn a_drop_after_a_key_press_lands(cx: &mut TestAppContext) {
    let (page, cx) = cx.add_window_view(|_, _| Page::default());
    let (source, target) = (
        point(px(SOURCE.0), px(SOURCE.1)),
        point(px(TARGET.0), px(TARGET.1)),
    );
    cx.simulate_mouse_down(source, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(target, MouseButton::Left, Modifiers::none());
    cx.simulate_keystrokes("a");
    cx.simulate_mouse_up(target, MouseButton::Left, Modifiers::none());
    page.read_with(cx, |page, _| assert_eq!(page.drops, 1));
}

#[crate::test]
fn a_file_dropped_after_a_key_press_lands(cx: &mut TestAppContext) {
    let (page, cx) = cx.add_window_view(|_, _| Page::default());
    let target = point(px(TARGET.0), px(TARGET.1));
    cx.simulate_keystrokes("a");
    cx.simulate_event(FileDropEvent::Entered {
        position: target,
        paths: ExternalPaths(vec![PathBuf::from("/tmp/dropped.txt")].into()),
    });
    cx.simulate_event(FileDropEvent::Submit { position: target });
    page.read_with(cx, |page, _| assert_eq!(page.file_drops, 1));
}

#[crate::test]
fn a_release_after_a_key_press_sets_mouse_modality(cx: &mut TestAppContext) {
    let (_, cx) = cx.add_window_view(|_, _| Page::default());
    let target = point(px(TARGET.0), px(TARGET.1));
    cx.simulate_mouse_down(target, MouseButton::Left, Modifiers::none());
    cx.simulate_keystrokes("a");
    cx.update(|window, _| assert!(window.last_input_was_keyboard()));
    cx.simulate_mouse_up(target, MouseButton::Left, Modifiers::none());
    cx.update(|window, _| assert!(!window.last_input_was_keyboard()));
}
