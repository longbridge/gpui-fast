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

// gpui-kit#4072: the hover style clears when an overlay closes under a still pointer.
mod hover_style_after_overlay {
    use crate::prelude::FluentBuilder as _;
    use crate::{
        AnyWindowHandle, AppContext, Context, InteractiveElement, IntoElement, ParentElement,
        Pixels, Render, Styled, TestAppContext, Window, canvas, div, point, px,
    };
    use std::{cell::Cell, rc::Rc};

    struct HoverStyleOverlayTestView {
        overlay_open: bool,
        width: Rc<Cell<Pixels>>,
    }

    impl Render for HoverStyleOverlayTestView {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let width = self.width.clone();
            div()
                .relative()
                .size_full()
                .child(
                    div()
                        .id("hover-style-row")
                        .absolute()
                        .top_0()
                        .left_0()
                        .size(px(10.))
                        .hover(|style| style.size(px(20.)))
                        .child(canvas(
                            move |bounds, _, _| width.set(bounds.size.width),
                            |_, _, _, _| {},
                        )),
                )
                .when(self.overlay_open, |root| {
                    root.child(div().absolute().top_0().left_0().size_full().occlude())
                })
        }
    }

    #[crate::test]
    fn hover_style_clears_after_overlay_closes_under_stationary_mouse(cx: &mut TestAppContext) {
        let width = Rc::new(Cell::new(px(0.)));
        let window = cx.add_window({
            let width = width.clone();
            move |_, _| HoverStyleOverlayTestView {
                overlay_open: true,
                width,
            }
        });
        let any_window = AnyWindowHandle::from(window);

        cx.update_window(any_window, |_, window, cx| {
            window.draw(cx).clear(cx);
            window.simulate_mouse_move(point(px(5.), px(5.)), cx);
        })
        .unwrap();
        assert_eq!(width.get(), px(10.));

        // The overlay closes with the pointer resting over the row: painted hovered, no mouse move.
        window
            .update(cx, |view, _, cx| {
                view.overlay_open = false;
                cx.notify();
            })
            .unwrap();
        cx.update_window(any_window, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        // Layout ran against the previous hit test; draw once more, as any later redraw would.
        window.update(cx, |_, window, _| window.refresh()).unwrap();
        cx.update_window(any_window, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        assert_eq!(width.get(), px(20.));

        // The first move already lands outside the row.
        cx.update_window(any_window, |_, window, cx| {
            window.simulate_mouse_move(point(px(50.), px(50.)), cx);
        })
        .unwrap();
        assert_eq!(width.get(), px(10.));

        // Entering and leaving again still works.
        cx.update_window(any_window, |_, window, cx| {
            window.simulate_mouse_move(point(px(5.), px(5.)), cx);
        })
        .unwrap();
        assert_eq!(width.get(), px(20.));
        cx.update_window(any_window, |_, window, cx| {
            window.simulate_mouse_move(point(px(50.), px(50.)), cx);
        })
        .unwrap();
        assert_eq!(width.get(), px(10.));
    }
}

// gpui-kit#3552: `group_hover` and `hover` styles clear when the pointer leaves
// the window straight from inside the group, with no mouse move outside it.
mod hover_style_after_window_exit {
    use crate::{
        AnyWindowHandle, AppContext, Context, InputEvent, InteractiveElement, IntoElement,
        MouseExitEvent, MouseMoveEvent, ParentElement, Pixels, Render, Styled, TestAppContext,
        Window, canvas, div, point, px,
    };
    use std::{cell::Cell, rc::Rc};

    struct GroupHoverView {
        group_width: Rc<Cell<Pixels>>,
        hover_width: Rc<Cell<Pixels>>,
    }

    impl Render for GroupHoverView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let group_width = self.group_width.clone();
            let hover_width = self.hover_width.clone();
            div().size_full().child(
                div()
                    .id("viewport")
                    .group("viewport")
                    .relative()
                    .size(px(50.))
                    .child(
                        div()
                            .absolute()
                            .size(px(10.))
                            .group_hover("viewport", |style| style.size(px(20.)))
                            .child(canvas(
                                move |bounds, _, _| group_width.set(bounds.size.width),
                                |_, _, _, _| {},
                            )),
                    )
                    .child(
                        div()
                            .id("hover-target")
                            .absolute()
                            .top(px(30.))
                            .size(px(10.))
                            .hover(|style| style.size(px(20.)))
                            .child(canvas(
                                move |bounds, _, _| hover_width.set(bounds.size.width),
                                |_, _, _, _| {},
                            )),
                    ),
            )
        }
    }

    #[crate::test]
    fn mouse_exit_clears_group_hover_and_hover(cx: &mut TestAppContext) {
        let group_width = Rc::new(Cell::new(px(0.)));
        let hover_width = Rc::new(Cell::new(px(0.)));
        let window = cx.add_window({
            let group_width = group_width.clone();
            let hover_width = hover_width.clone();
            move |_, _| GroupHoverView {
                group_width,
                hover_width,
            }
        });
        let window = AnyWindowHandle::from(window);
        let inside = point(px(5.), px(35.));

        cx.update_window(window, |_, window, cx| {
            window.draw(cx).clear(cx);
            assert_eq!(group_width.get(), px(10.));
            assert_eq!(hover_width.get(), px(10.));

            window.dispatch_event(
                MouseMoveEvent {
                    position: inside,
                    ..Default::default()
                }
                .to_platform_input(),
                cx,
            );
            window.draw(cx).clear(cx);
            assert_eq!(group_width.get(), px(20.));
            assert_eq!(hover_width.get(), px(20.));

            // No refresh: the exit itself has to redraw the styles.
            window.dispatch_event(
                MouseExitEvent {
                    position: inside,
                    ..Default::default()
                }
                .to_platform_input(),
                cx,
            );
            window.draw(cx).clear(cx);
            assert_eq!(group_width.get(), px(10.));
            assert_eq!(hover_width.get(), px(10.));
        })
        .unwrap();
    }
}
