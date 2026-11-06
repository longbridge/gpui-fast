//! Tests of notifications that a wheel event's other listeners send for its
//! scroll. See [`crate::fast::layers::wheel`].

use crate::{
    AppContext as _, Bounds, Context, InteractiveElement as _, IntoElement, ParentElement as _,
    Pixels, PlatformInput, Point, Render, ScrollDelta, ScrollEvent, ScrollHandle,
    StatefulInteractiveElement as _, Styled as _, TestAppContext, TouchPhase, Window, WindowHandle,
    canvas, div, point, px, rgb, size,
};
use std::{cell::Cell, rc::Rc};

const ROWS: u32 = 40;

fn viewport() -> Bounds<Pixels> {
    Bounds::new(point(px(0.), px(0.)), size(px(200.), px(100.)))
}

/// A scroll container of rows, with a scrollbar beside it that listens to
/// the wheel as GPUI Kit's does: when the offset moved since it last saw it,
/// it notes the new one outside entities and notifies the view again. With
/// `update`, it instead counts the scroll in the view's state, which the
/// first row shows, and notifies it.
struct Page {
    handle: ScrollHandle,
    seen: Rc<Cell<Point<Pixels>>>,
    update: bool,
    wheel_scrolls: u32,
}

impl Render for Page {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let scroller = div()
            .id("scroller")
            .overflow_y_scroll()
            .track_scroll(&self.handle)
            .w(viewport().size.width)
            .h(viewport().size.height)
            .children((0..ROWS).map(|index| {
                let tint = if index == 0 {
                    self.wheel_scrolls * 0x100
                } else {
                    0
                };
                div()
                    .w(px(180.))
                    .h(px(20.))
                    .bg(rgb(0x100000 + index * 0x10 + tint))
            }));
        let handle = self.handle.clone();
        let seen = self.seen.clone();
        let update = self.update.then(|| cx.entity().downgrade());
        let scrollbar = canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                let offset = handle.offset().y;
                let thumb = Bounds::new(
                    point(bounds.origin.x, bounds.origin.y - offset / 8.),
                    size(px(6.), px(20.)),
                );
                window.paint_quad(crate::fill(thumb, rgb(0x888888)));
                let view = window.current_view();
                window.on_mouse_event(move |event: &ScrollEvent, phase, _, cx| {
                    if phase.bubble()
                        && viewport().contains(&event.position)
                        && handle.offset() != seen.get()
                    {
                        seen.set(handle.offset());
                        match &update {
                            Some(page) => {
                                let _ = page.update(cx, |page, cx| {
                                    page.wheel_scrolls += 1;
                                    cx.notify();
                                });
                            }
                            None => cx.notify(view),
                        }
                    }
                });
            },
        )
        .absolute()
        .left(viewport().size.width)
        .top(px(0.))
        .w(px(6.))
        .h(viewport().size.height);
        div()
            .relative()
            .size_full()
            .bg(rgb(0xffffff))
            .child(scroller)
            .child(scrollbar)
    }
}

fn open(cx: &mut TestAppContext, update: bool, layers: bool) -> WindowHandle<Page> {
    let window = cx.add_window(move |_, _| Page {
        handle: ScrollHandle::new(),
        seen: Rc::new(Cell::new(Point::default())),
        update,
        wheel_scrolls: 0,
    });
    cx.update_window(window.into(), |_, window, cx| {
        window.set_scroll_layers(layers);
        window.draw(cx).clear(cx);
    })
    .unwrap();
    window
}

/// Whether the scroll container composited its layer in the last frame.
fn composited(window: &mut Window) -> bool {
    let id = window
        .fast_layers
        .scrolls
        .containers()
        .find(|id| id.last() == Some(&"scroller".into()))
        .cloned()
        .expect("the scroll container was painted");
    crate::fast::layers::policy::last_decision(window, &id)
        == Some(crate::fast::layers::policy::Decision::Composite)
}

/// Scrolls a window with layers and one without through the same frames,
/// each taking `events` wheel events of a few pixels, and checks that every
/// frame draws the same. Returns how many frames composited the layer.
fn scroll_both(cx: &mut TestAppContext, update: bool, events: usize) -> usize {
    let layered = open(cx, update, true);
    let plain = open(cx, update, false);
    let mut composited_frames = 0;
    for (step, dy) in [-7., -5., -3., -6., -4., -2., 3., -5., -1., -8., -3., -4.]
        .into_iter()
        .enumerate()
    {
        let drawn = [layered, plain].map(|window| {
            cx.update_window(window.into(), |_, window, cx| {
                for _ in 0..events {
                    window.dispatch_event(
                        PlatformInput::Scroll(ScrollEvent {
                            position: point(px(20.), px(20.)),
                            delta: ScrollDelta::Pixels(point(px(0.), px(dy))),
                            modifiers: Default::default(),
                            touch_phase: TouchPhase::Moved,
                            ..Default::default()
                        }),
                        cx,
                    );
                }
                window.draw(cx).clear(cx);
                window.painted_primitives()
            })
            .unwrap()
        });
        assert_eq!(drawn[0], drawn[1], "step {step}, scrolled by {dy}");
        if cx
            .update_window(layered.into(), |_, window, _| composited(window))
            .unwrap()
        {
            composited_frames += 1;
        }
    }
    composited_frames
}

/// A scrollbar's wheel listener notifying the view again for a scroll, with
/// one wheel event a frame or several, does not keep the content from being
/// composited.
#[test]
fn a_scrollbar_notifying_for_the_scroll_composites() {
    if !crate::fast::layers::COMPILED {
        return;
    }
    for events in [1, 3] {
        let mut cx = TestAppContext::single();
        let composited = scroll_both(&mut cx, false, events);
        assert!(
            composited >= 8,
            "{events} events a frame: composited {composited} frames"
        );
    }
}

/// A wheel listener that changes the view's state and notifies it changed
/// the content: the layer is not composited, and every frame draws what a
/// window without layers draws.
#[test]
fn a_wheel_listener_updating_an_entity_does_not_composite() {
    if !crate::fast::layers::COMPILED {
        return;
    }
    for events in [1, 3] {
        let mut cx = TestAppContext::single();
        let composited = scroll_both(&mut cx, true, events);
        assert_eq!(composited, 0, "{events} events a frame");
    }
}

/// A horizontal scroll container of a fixed width, inside a page. Counts its
/// renders.
struct Strip {
    renders: Rc<Cell<usize>>,
    handle: ScrollHandle,
}

impl Render for Strip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.renders.set(self.renders.get() + 1);
        div().size_full().child(
            div()
                .id("strip")
                .track_scroll(&self.handle)
                .overflow_x_scroll()
                .flex()
                .w(px(100.))
                .h(px(50.))
                .child(div().flex_none().w(px(160.)).h(px(50.))),
        )
    }
}

struct StripPage(crate::Entity<Strip>);

impl Render for StripPage {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.0.clone())
    }
}

/// A wheel that cannot move a scroll container's content, because it is
/// already at the end the wheel points to, changes nothing: the view that
/// painted the container is not built again. A vertical wheel over a
/// horizontal container, as a page scrolled past a carousel sends it on every
/// event, is mapped onto its one axis and is the usual case.
#[test]
fn a_wheel_that_cannot_move_its_container_does_not_build_its_view() {
    let mut cx = TestAppContext::single();
    let renders = Rc::new(Cell::new(0));
    let handle = ScrollHandle::new();
    let window = cx.add_window({
        let (renders, handle) = (renders.clone(), handle.clone());
        move |_, cx| StripPage(cx.new(|_| Strip { renders, handle }))
    });
    let draw = |cx: &mut TestAppContext| {
        cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
    };
    draw(&mut cx);
    let wheel = |cx: &mut TestAppContext, dy: f32| {
        let before = renders.get();
        cx.update_window(window.into(), |_, window, cx| {
            window.dispatch_event(
                PlatformInput::Scroll(ScrollEvent {
                    position: point(px(50.), px(25.)),
                    delta: ScrollDelta::Pixels(point(px(0.), px(dy))),
                    modifiers: Default::default(),
                    touch_phase: TouchPhase::Moved,
                    ..Default::default()
                }),
                cx,
            );
        })
        .unwrap();
        draw(cx);
        renders.get() - before
    };
    // The window's first wheel changes its input modality, which draws it
    // again whole.
    wheel(&mut cx, 20.);

    assert_eq!(wheel(&mut cx, 20.), 0, "a wheel towards the start it is at");
    assert_eq!(handle.offset().x, px(0.));
    assert_eq!(wheel(&mut cx, -20.), 1, "a wheel with room");
    assert_eq!(handle.offset().x, px(-20.));
    assert_eq!(wheel(&mut cx, -60.), 1, "a wheel past the end");
    assert_eq!(handle.offset().x, px(-60.));
    assert_eq!(wheel(&mut cx, -20.), 0, "a wheel towards the end it is at");
    assert_eq!(handle.offset().x, px(-60.));
}
