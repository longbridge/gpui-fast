//! Tests of views drawn again from what they drew on the last frame. See
//! [`crate::fast::retained`].

use crate::{
    AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, StyleRefinement, Styled as _, TestAppContext, Window, WindowHandle, div,
    prelude::FluentBuilder as _, px,
};
use std::{cell::Cell, rc::Rc};

struct Row {
    label: u32,
    builds: Rc<Cell<usize>>,
}

impl Render for Row {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.builds.set(self.builds.get() + 1);
        div()
            .size_full()
            .bg(crate::black())
            .hover(|style| style.bg(crate::white()))
            .child(format!("row {}", self.label))
    }
}

struct Rows {
    row: Entity<Row>,
    covered: bool,
}

impl Render for Rows {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .relative()
            .size(px(300.))
            .child(
                self.row
                    .clone()
                    .cached(StyleRefinement::default().w(px(100.)).h(px(20.))),
            )
            .when(self.covered, |this| {
                this.child(div().absolute().top_0().left_0().size(px(200.)).occlude())
            })
    }
}

fn window(cx: &mut TestAppContext) -> (WindowHandle<Rows>, Entity<Row>, Rc<Cell<usize>>) {
    let builds = Rc::new(Cell::new(0));
    let window = cx.add_window({
        let builds = builds.clone();
        move |_, cx| Rows {
            row: cx.new(|_| Row { label: 0, builds }),
            covered: false,
        }
    });
    let row = window.update(cx, |rows, _, _| rows.row.clone()).unwrap();
    (window, row, builds)
}

fn draw(cx: &mut TestAppContext, window: WindowHandle<Rows>) -> Vec<String> {
    cx.update_window(window.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        window.describe_rendered_frame()
    })
    .unwrap()
}

fn notify_parent(cx: &mut TestAppContext, window: WindowHandle<Rows>) {
    window.update(cx, |_, _, cx| cx.notify()).unwrap();
}

fn move_mouse(cx: &mut TestAppContext, window: WindowHandle<Rows>, x: f32, y: f32) {
    cx.update_window(window.into(), |_, window, cx| {
        window.simulate_mouse_move(crate::point(px(x), px(y)), cx);
    })
    .unwrap();
}

/// A cached view painted while the pointer was over something in it that
/// has a hover style is rendered again once the pointer leaves, even
/// though the element never saw the pointer arrive.
#[test]
fn a_cached_view_is_rendered_again_when_a_hover_it_was_painted_by_changes() {
    let mut cx = TestAppContext::single();
    let (window, _, builds) = window(&mut cx);
    move_mouse(&mut cx, window, 10., 10.);
    let hovered = draw(&mut cx, window);
    notify_parent(&mut cx, window);
    draw(&mut cx, window);
    let builds_before = builds.get();

    move_mouse(&mut cx, window, 250., 250.);
    let left = draw(&mut cx, window);
    assert_eq!(builds.get(), builds_before + 1);
    assert_ne!(hovered, left);
}

/// A cached view reused for a while keeps the layout nodes it was laid
/// out with, so rendering it again finds them all.
#[test]
fn a_reused_cached_view_keeps_its_layout_nodes() {
    let mut cx = TestAppContext::single();
    let (window, row, builds) = window(&mut cx);
    draw(&mut cx, window);
    for _ in 0..3 {
        notify_parent(&mut cx, window);
        draw(&mut cx, window);
    }
    assert_eq!(
        builds.get(),
        1,
        "the view is reused while its parent renders"
    );

    cx.update_window(window.into(), |_, window, _| window.reset_layout_stats())
        .unwrap();
    row.update(&mut cx, |row, cx| {
        row.label = 7;
        cx.notify();
    });
    draw(&mut cx, window);
    assert_eq!(builds.get(), 2);
    let stats = cx
        .update_window(window.into(), |_, window, _| window.layout_stats())
        .unwrap();
    assert_eq!(
        stats.nodes_created, 0,
        "the view's nodes should have been kept while it was reused"
    );
    assert!(stats.nodes_reused > 0);
}

/// Something drawn over a hovered cached view is found out only when the
/// view paints; it is rendered on the next frame, which is asked for.
#[test]
fn a_cached_view_covered_while_hovered_is_rendered_on_the_next_frame() {
    let mut cx = TestAppContext::single();
    let (window, _, builds) = window(&mut cx);
    move_mouse(&mut cx, window, 10., 10.);
    let hovered = draw(&mut cx, window);
    notify_parent(&mut cx, window);
    draw(&mut cx, window);
    let builds_before = builds.get();

    window
        .update(&mut cx, |rows, _, cx| {
            rows.covered = true;
            cx.notify();
        })
        .unwrap();
    let frame_asked_for = |cx: &mut TestAppContext| {
        cx.update_window(window.into(), |_, window, _| {
            !window.next_frame_callbacks.borrow().is_empty()
        })
        .unwrap()
    };
    let mut asked_for_a_frame = frame_asked_for(&mut cx);
    let mut look = None;
    for _ in 0..3 {
        if builds.get() > builds_before {
            break;
        }
        asked_for_a_frame |= frame_asked_for(&mut cx);
        look = Some(draw(&mut cx, window));
    }
    assert_eq!(builds.get(), builds_before + 1);
    assert!(
        asked_for_a_frame,
        "a frame should be asked for to render it"
    );
    assert_ne!(Some(hovered), look);
}

struct Counted {
    label: usize,
    model: Option<Entity<Model>>,
    builds: Rc<Cell<usize>>,
}

struct Model(usize);

impl Render for Counted {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.builds.set(self.builds.get() + 1);
        let model = self.model.as_ref().map_or(0, |model| model.read(cx).0);
        div()
            .flex()
            .flex_row()
            .child(format!("{} {}", self.label, model))
    }
}

struct Siblings {
    first: Entity<Counted>,
    second: Entity<Counted>,
    spacer: f32,
}

impl Render for Siblings {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .child(div().h(px(self.spacer)))
            .child(self.first.clone())
            .child(self.second.clone())
    }
}

struct SiblingsWindow {
    window: WindowHandle<Siblings>,
    first: Entity<Counted>,
    model: Entity<Model>,
    first_builds: Rc<Cell<usize>>,
    second_builds: Rc<Cell<usize>>,
}

fn siblings(cx: &mut TestAppContext) -> SiblingsWindow {
    let first_builds = Rc::new(Cell::new(0));
    let second_builds = Rc::new(Cell::new(0));
    let model = cx.new(|_| Model(0));
    let window = cx.add_window({
        let (first_builds, second_builds, model) =
            (first_builds.clone(), second_builds.clone(), model.clone());
        move |_, cx| Siblings {
            first: cx.new(|_| Counted {
                label: 1,
                model: None,
                builds: first_builds,
            }),
            second: cx.new(|_| Counted {
                label: 2,
                model: Some(model),
                builds: second_builds,
            }),
            spacer: 10.,
        }
    });
    let first = window.update(cx, |view, _, _| view.first.clone()).unwrap();
    SiblingsWindow {
        window,
        first,
        model,
        first_builds,
        second_builds,
    }
}

fn draw_siblings(cx: &mut TestAppContext, window: WindowHandle<Siblings>) -> Vec<String> {
    cx.update_window(window.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        window.describe_rendered_frame()
    })
    .unwrap()
}

/// A view that is not cached is drawn again from the
/// last frame while nothing it read changed, even when the view around it
/// is rendered again, and rendered again once something it read did.
#[test]
fn a_view_is_rendered_again_only_when_something_it_read_changed() {
    let mut cx = TestAppContext::single();
    let s = siblings(&mut cx);
    draw_siblings(&mut cx, s.window);
    assert_eq!((s.first_builds.get(), s.second_builds.get()), (1, 1));

    s.window.update(&mut cx, |_, _, cx| cx.notify()).unwrap();
    draw_siblings(&mut cx, s.window);
    assert_eq!(
        (s.first_builds.get(), s.second_builds.get()),
        (1, 1),
        "notifying the parent leaves its children alone"
    );

    s.first.update(&mut cx, |first, cx| {
        first.label = 3;
        cx.notify();
    });
    draw_siblings(&mut cx, s.window);
    assert_eq!((s.first_builds.get(), s.second_builds.get()), (2, 1));

    s.model.update(&mut cx, |model, cx| {
        model.0 = 5;
        cx.notify();
    });
    draw_siblings(&mut cx, s.window);
    assert_eq!(
        (s.first_builds.get(), s.second_builds.get()),
        (2, 2),
        "a model the view read changing renders it again, unobserved"
    );

    cx.update_window(s.window.into(), |_, window, _| window.refresh())
        .unwrap();
    draw_siblings(&mut cx, s.window);
    assert_eq!((s.first_builds.get(), s.second_builds.get()), (3, 3));
}

/// A view that moved is built again where it went, at the layout nodes it
/// kept, and draws what a window drawing from scratch draws.
#[test]
fn a_moved_view_is_built_again_at_its_layout() {
    let mut cx = TestAppContext::single();
    let s = siblings(&mut cx);
    draw_siblings(&mut cx, s.window);
    s.window
        .update(&mut cx, |view, _, cx| {
            view.spacer = 30.;
            cx.notify();
        })
        .unwrap();
    cx.update_window(s.window.into(), |_, window, _| window.reset_layout_stats())
        .unwrap();
    let moved = draw_siblings(&mut cx, s.window);
    assert_eq!((s.first_builds.get(), s.second_builds.get()), (2, 2));
    let stats = cx
        .update_window(s.window.into(), |_, window, _| window.layout_stats())
        .unwrap();
    assert_eq!(stats.nodes_created, 0, "the moved views keep their nodes");

    cx.update_window(s.window.into(), |_, window, _| {
        window.forget_retained_state()
    })
    .unwrap();
    assert_eq!(moved, draw_siblings(&mut cx, s.window));
}

/// With retention turned off, every view is rendered every frame.
#[test]
fn views_are_rendered_every_frame_without_retention() {
    let mut cx = TestAppContext::single();
    let s = siblings(&mut cx);
    cx.update_window(s.window.into(), |_, window, _| {
        window.set_view_retention(false)
    })
    .unwrap();
    draw_siblings(&mut cx, s.window);
    let before = (s.first_builds.get(), s.second_builds.get());
    s.window.update(&mut cx, |_, _, cx| cx.notify()).unwrap();
    draw_siblings(&mut cx, s.window);
    assert!(s.first_builds.get() > before.0 && s.second_builds.get() > before.1);
}

struct Sized {
    row: Entity<Row>,
    width: f32,
}

impl Render for Sized {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().child(
            self.row
                .clone()
                .cached(StyleRefinement::default().w(px(self.width)).h(px(20.))),
        )
    }
}

/// A cached view is rendered again when its bounds change or the window is
/// refreshed, and otherwise reused, showing what it showed.
#[test]
fn a_cached_view_is_rendered_again_only_when_it_has_to_be() {
    let mut cx = TestAppContext::single();
    let builds = Rc::new(Cell::new(0));
    let window = cx.add_window({
        let builds = builds.clone();
        move |_, cx| Sized {
            row: cx.new(|_| Row { label: 0, builds }),
            width: 100.,
        }
    });
    let draw = |cx: &mut TestAppContext| {
        cx.update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            window.describe_rendered_frame()
        })
        .unwrap()
    };
    let change = |cx: &mut TestAppContext, f: fn(&mut Sized)| {
        window
            .update(cx, |view, _, cx| {
                f(view);
                cx.notify();
            })
            .unwrap()
    };

    let first = draw(&mut cx);
    assert_eq!(builds.get(), 1);
    change(&mut cx, |_| {});
    assert_eq!(first, draw(&mut cx), "a reused view shows what it showed");
    assert_eq!(builds.get(), 1);

    change(&mut cx, |view| view.width = 150.);
    draw(&mut cx);
    assert_eq!(builds.get(), 2, "new bounds render it again");

    cx.update_window(window.into(), |_, window, _| window.refresh())
        .unwrap();
    draw(&mut cx);
    assert_eq!(builds.get(), 3, "a refreshed window renders every view");

    cx.simulate_window_resize(window.into(), crate::size(px(800.), px(600.)));
    draw(&mut cx);
    change(&mut cx, |_| {});
    draw(&mut cx);
    assert_eq!(
        builds.get(),
        4,
        "a resize refreshes once, then it is reused"
    );
}

struct PopoverRow {
    builds: Rc<Cell<usize>>,
}

impl Render for PopoverRow {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.builds.set(self.builds.get() + 1);
        div().size_full().child(crate::deferred(
            div()
                .id("popover")
                .w(px(50.))
                .h(px(20.))
                .bg(crate::black())
                .hover(|style| style.bg(crate::white())),
        ))
    }
}

struct WithPopover {
    row: Entity<PopoverRow>,
}

impl Render for WithPopover {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().child(
            self.row
                .clone()
                .cached(StyleRefinement::default().w(px(100.)).h(px(20.))),
        )
    }
}

/// Something a cached view draws deferred, such as a popover, is prepainted
/// and painted after the view, but is part of what the view drew: the pointer
/// moving over it changes how the view looks.
#[test]
fn a_cached_view_is_rendered_again_when_the_pointer_moves_over_what_it_deferred() {
    let mut cx = TestAppContext::single();
    let builds = Rc::new(Cell::new(0));
    let window = cx.add_window({
        let builds = builds.clone();
        move |_, cx| WithPopover {
            row: cx.new(|_| PopoverRow { builds }),
        }
    });
    let draw = |cx: &mut TestAppContext| {
        cx.update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            window.describe_rendered_frame()
        })
        .unwrap()
    };
    let move_to = |cx: &mut TestAppContext, x: f32, y: f32| {
        cx.update_window(window.into(), |_, window, cx| {
            window.simulate_mouse_move(crate::point(px(x), px(y)), cx);
        })
        .unwrap();
    };

    // The pointer starts out at the origin, over the popover.
    draw(&mut cx);
    assert_eq!(builds.get(), 1);
    move_to(&mut cx, 500., 500.);
    let away = draw(&mut cx);
    assert_eq!(
        builds.get(),
        2,
        "the pointer leaving the popover changes its look"
    );
    window.update(&mut cx, |_, _, cx| cx.notify()).unwrap();
    draw(&mut cx);
    assert_eq!(builds.get(), 2, "and nothing else does");

    move_to(&mut cx, 10., 10.);
    let over = draw(&mut cx);
    assert_eq!(builds.get(), 3, "the pointer coming back changes it again");
    assert_ne!(away, over);

    move_to(&mut cx, 500., 500.);
    let away_again = draw(&mut cx);
    assert_eq!(builds.get(), 4);
    assert_eq!(away, away_again);
}

/// An input handler that answers every question about its text with its
/// name, so a test can tell which one the platform was handed.
struct NamedInput(&'static str);

impl crate::InputHandler for NamedInput {
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut crate::App,
    ) -> Option<crate::UTF16Selection> {
        None
    }
    fn marked_text_range(
        &mut self,
        _: &mut Window,
        _: &mut crate::App,
    ) -> Option<std::ops::Range<usize>> {
        None
    }
    fn text_for_range(
        &mut self,
        _: std::ops::Range<usize>,
        _: &mut Option<std::ops::Range<usize>>,
        _: &mut Window,
        _: &mut crate::App,
    ) -> Option<String> {
        Some(self.0.to_string())
    }
    fn replace_text_in_range(
        &mut self,
        _: Option<std::ops::Range<usize>>,
        _: &str,
        _: &mut Window,
        _: &mut crate::App,
    ) {
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<std::ops::Range<usize>>,
        _: &str,
        _: Option<std::ops::Range<usize>>,
        _: &mut Window,
        _: &mut crate::App,
    ) {
    }
    fn unmark_text(&mut self, _: &mut Window, _: &mut crate::App) {}
    fn bounds_for_range(
        &mut self,
        _: std::ops::Range<usize>,
        _: &mut Window,
        _: &mut crate::App,
    ) -> Option<crate::Bounds<crate::Pixels>> {
        None
    }
    fn character_index_for_point(
        &mut self,
        _: crate::Point<crate::Pixels>,
        _: &mut Window,
        _: &mut crate::App,
    ) -> Option<usize> {
        None
    }
}

fn text_input(focus: crate::FocusHandle, name: &'static str) -> impl IntoElement {
    crate::canvas(
        |_, _, _| {},
        move |_, _, window, cx| window.handle_input(&focus, NamedInput(name), cx),
    )
    .size_full()
}

struct Field {
    focus: crate::FocusHandle,
    builds: Rc<Cell<usize>>,
}

impl Render for Field {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.builds.set(self.builds.get() + 1);
        text_input(self.focus.clone(), "inside")
    }
}

struct Inputs {
    field: Entity<Field>,
    outside: crate::FocusHandle,
    cached_first: bool,
}

impl Render for Inputs {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let cached = self
            .field
            .clone()
            .cached(StyleRefinement::default().w(px(100.)).h(px(20.)))
            .into_any_element();
        let outside = div()
            .w(px(100.))
            .h(px(20.))
            .child(text_input(self.outside.clone(), "outside"))
            .into_any_element();
        let children = if self.cached_first {
            [cached, outside]
        } else {
            [outside, cached]
        };
        div().children(children)
    }
}

/// The input handler of a focused field inside a reused cached view is
/// handed to the platform on every frame, and a field focused elsewhere
/// takes over.
#[test]
fn a_reused_cached_view_hands_the_platform_its_input_handler() {
    for cached_first in [true, false] {
        let mut cx = TestAppContext::single();
        let builds = Rc::new(Cell::new(0));
        let window = cx.add_window({
            let builds = builds.clone();
            move |_, cx| Inputs {
                field: cx.new(|cx| Field {
                    focus: cx.focus_handle(),
                    builds,
                }),
                outside: cx.focus_handle(),
                cached_first,
            }
        });
        let (inside, outside) = window
            .update(&mut cx, |view, _, cx| {
                (view.field.read(cx).focus.clone(), view.outside.clone())
            })
            .unwrap();
        let draw = |cx: &mut TestAppContext| {
            cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
                .unwrap()
        };
        // Asked outside of any update, as the platform asks it.
        let handed = |cx: &mut TestAppContext| {
            let mut handler = cx
                .update_window(window.into(), |_, window, _| {
                    window.platform_window.take_input_handler()
                })
                .unwrap()?;
            let name = handler.text_for_range(0..1, &mut None);
            cx.update_window(window.into(), |_, window, _| {
                window.platform_window.set_input_handler(handler)
            })
            .unwrap();
            name
        };
        let focus = |cx: &mut TestAppContext, handle: &crate::FocusHandle| {
            cx.update_window(window.into(), |_, window, cx| window.focus(handle, cx))
                .unwrap()
        };

        focus(&mut cx, &inside);
        draw(&mut cx);
        assert_eq!(handed(&mut cx).as_deref(), Some("inside"));
        let builds_then = builds.get();
        for _ in 0..3 {
            window.update(&mut cx, |_, _, cx| cx.notify()).unwrap();
            draw(&mut cx);
            assert_eq!(handed(&mut cx).as_deref(), Some("inside"));
        }
        assert_eq!(builds.get(), builds_then, "the cached view was reused");

        focus(&mut cx, &outside);
        draw(&mut cx);
        assert_eq!(handed(&mut cx).as_deref(), Some("outside"));
        window.update(&mut cx, |_, _, cx| cx.notify()).unwrap();
        draw(&mut cx);
        assert_eq!(handed(&mut cx).as_deref(), Some("outside"));

        focus(&mut cx, &inside);
        draw(&mut cx);
        window.update(&mut cx, |_, _, cx| cx.notify()).unwrap();
        draw(&mut cx);
        assert_eq!(handed(&mut cx).as_deref(), Some("inside"));
    }
}

/// A card whose width comes from the column it sits in, not from its content.
struct Card;

impl Render for Card {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .items_center()
            .child(div().h(px(10.)).bg(crate::black()).child("card"))
    }
}

struct Stretched {
    card: Entity<Card>,
    spacer: f32,
}

impl Render for Stretched {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .w(px(300.))
            .child(div().h(px(self.spacer)))
            .child(self.card.clone())
    }
}

/// A view stretched by the column it is in keeps that width when it moves,
/// as when a scrolled list moves every view in it: it is laid out again at
/// the size its parent gave it, not at the size its content asks for.
#[test]
fn a_moved_view_keeps_the_size_its_parent_gave_it() {
    let mut cx = TestAppContext::single();
    let window = cx.add_window(|_, cx| Stretched {
        card: cx.new(|_| Card),
        spacer: 10.,
    });
    let draw = |cx: &mut TestAppContext| {
        cx.update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            window.describe_rendered_frame()
        })
        .unwrap()
    };
    draw(&mut cx);
    window
        .update(&mut cx, |view, _, cx| {
            view.spacer = 30.;
            cx.notify();
        })
        .unwrap();
    let moved = draw(&mut cx);
    cx.update_window(window.into(), |_, window, _| window.forget_retained_state())
        .unwrap();
    assert_eq!(moved, draw(&mut cx));
}

/// A view that read a model updated without being notified is built again
/// when the view around it is, as upstream builds every view under a
/// notified one again: a view often changes a model it renders and notifies
/// only itself.
#[test]
fn a_view_is_rendered_again_when_a_model_it_read_was_updated_without_a_notify() {
    let mut cx = TestAppContext::single();
    let s = siblings(&mut cx);
    draw_siblings(&mut cx, s.window);
    assert_eq!((s.first_builds.get(), s.second_builds.get()), (1, 1));

    s.model.update(&mut cx, |model, _| model.0 = 7);
    s.window.update(&mut cx, |_, _, cx| cx.notify()).unwrap();
    let updated = draw_siblings(&mut cx, s.window);
    assert_eq!(
        (s.first_builds.get(), s.second_builds.get()),
        (1, 2),
        "only the view that read the model is built again"
    );

    s.window.update(&mut cx, |_, _, cx| cx.notify()).unwrap();
    draw_siblings(&mut cx, s.window);
    assert_eq!(
        (s.first_builds.get(), s.second_builds.get()),
        (1, 2),
        "an update is seen once"
    );

    cx.update_window(s.window.into(), |_, window, _| {
        window.forget_retained_state()
    })
    .unwrap();
    assert_eq!(updated, draw_siblings(&mut cx, s.window));
}

/// A view built again keeps the measurements of the text that did not
/// change, rather than measuring and laying it out again, and measures the
/// text that did.
#[test]
fn text_that_did_not_change_keeps_its_measurement() {
    let mut cx = TestAppContext::single();
    let s = siblings(&mut cx);
    draw_siblings(&mut cx, s.window);
    let stats = |cx: &mut TestAppContext| {
        cx.update_window(s.window.into(), |_, window, _| window.layout_stats())
            .unwrap()
    };
    let reset = |cx: &mut TestAppContext| {
        cx.update_window(s.window.into(), |_, window, _| window.reset_layout_stats())
            .unwrap()
    };

    reset(&mut cx);
    s.window
        .update(&mut cx, |view, _, cx| {
            view.spacer = 30.;
            cx.notify();
        })
        .unwrap();
    draw_siblings(&mut cx, s.window);
    let after_move = stats(&mut cx);
    assert_eq!(
        after_move.measure_rebinds, 0,
        "moved text is not measured again"
    );
    assert_eq!(after_move.measurements_kept, 2);

    reset(&mut cx);
    s.first.update(&mut cx, |first, cx| {
        first.label = 9;
        cx.notify();
    });
    let changed = draw_siblings(&mut cx, s.window);
    let after_change = stats(&mut cx);
    assert_eq!(
        after_change.measure_rebinds, 1,
        "changed text is measured again"
    );

    cx.update_window(s.window.into(), |_, window, _| {
        window.forget_retained_state()
    })
    .unwrap();
    assert_eq!(changed, draw_siblings(&mut cx, s.window));
}
