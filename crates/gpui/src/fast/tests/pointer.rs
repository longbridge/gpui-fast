//! `fast::pointer`: hover clears when the pointer leaves through the window edge.

use crate::{AppContext, InputEvent, InteractiveElement};
use crate::{
    Context, IntoElement, MouseMoveEvent, ParentElement, Render, Styled, TestAppContext, Window,
    div, point, px,
};

#[crate::test]
fn test_mouse_exited_on_window_edge_clears_hover(cx: &mut TestAppContext) {
    // A control flush with the window edge: the exit position (on the edge) is
    // still inside its bounds, so hit testing must ignore it after MouseExited.
    struct Controls;

    impl Render for Controls {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(
                div()
                    .id("close")
                    .size(px(40.))
                    .bg(crate::black())
                    .hover(|style| style.bg(crate::red())),
            )
        }
    }

    let window = cx.add_window(|_, _| Controls);
    let edge = point(px(10.), px(0.));
    let background = |window: &Window| {
        window
            .rendered_frame
            .scene
            .quads
            .first()
            .expect("close quad")
            .background
            .solid
    };

    cx.update_window(window.into(), |_, window, cx| {
        window.hovered.set(true);
        window.draw(cx).clear(cx);
        window.dispatch_event(
            MouseMoveEvent {
                position: edge,
                ..Default::default()
            }
            .to_platform_input(),
            cx,
        );
        window.draw(cx).clear(cx);
        assert_eq!(background(window), crate::red());

        window.dispatch_event(
            crate::MouseExitEvent {
                position: edge,
                ..Default::default()
            }
            .to_platform_input(),
            cx,
        );
        window.hovered.set(false);
        window.refresh();
        window.draw(cx).clear(cx);
        assert_eq!(background(window), crate::black());

        // Re-entering hovers again.
        window.hovered.set(true);
        window.dispatch_event(
            MouseMoveEvent {
                position: edge,
                ..Default::default()
            }
            .to_platform_input(),
            cx,
        );
        window.draw(cx).clear(cx);
        assert_eq!(background(window), crate::red());
    })
    .expect("update window");
}
