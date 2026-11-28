//! An animated image wakes its view when its next frame is due, instead of
//! asking for an animation frame on every paint, and keeps playing in
//! content scrolled through a layer. See [`crate::fast::animated_image`];
//! that it keeps its scroll container eligible for a layer is tested with the
//! other policies, in `layers.rs`.

use crate::{
    AnyWindowHandle, AppContext as _, Bounds, Context, Entity, InteractiveElement as _,
    IntoElement, ParentElement as _, PlatformInput, Render, RenderImage, ScaledPixels, ScrollDelta,
    ScrollEvent, StatefulInteractiveElement as _, Styled as _, TestAppContext, TouchPhase, Window,
    WindowHandle, div, img, point, px, rgb,
};
use image::{Delay, Frame, ImageBuffer, Rgba};
use std::{cell::Cell, rc::Rc, sync::Arc, time::Duration};

pub(super) const FRAME_DELAY: Duration = Duration::from_millis(100);
const MIN_FRAME_DELAY: Duration = Duration::from_millis(16);
const MILLISECOND: Duration = Duration::from_millis(1);

/// Three frames shown for `delay` each, of different widths, so the sprite
/// of a square `img` shows which frame it is on.
pub(super) fn gif(delay: Duration) -> Arc<RenderImage> {
    let delay = Delay::from_saturating_duration(delay);
    Arc::new(RenderImage::new(
        (1..=3)
            .map(|width| {
                let buffer = ImageBuffer::from_pixel(width * 10, 10, Rgba([255, 0, 0, 255]));
                Frame::from_parts(buffer, 0, 0, delay)
            })
            .collect::<Vec<_>>(),
    ))
}

/// Runs the tasks that are ready. Not `run_until_parked`, which would move
/// the clock on to an animated image's next timer, and every one after it.
pub(super) fn run_ready(cx: &mut TestAppContext) {
    while cx.executor().tick() {}
}

/// An animated image in a view of its own, counting its renders.
struct Gif {
    image: Arc<RenderImage>,
    renders: Rc<Cell<usize>>,
}

impl Render for Gif {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.renders.set(self.renders.get() + 1);
        img(self.image.clone()).id("gif").size(px(30.))
    }
}

/// Opens an active window showing a GIF whose frames show for `delay`, and
/// draws it, which starts the animation's clock.
fn open(cx: &mut TestAppContext, delay: Duration) -> (AnyWindowHandle, Rc<Cell<usize>>) {
    let renders = Rc::new(Cell::new(0));
    let window = cx
        .add_window({
            let renders = renders.clone();
            move |_, _| Gif {
                image: gif(delay),
                renders,
            }
        })
        .into();
    cx.update_window(window, |_, window, _| window.activate_window())
        .unwrap();
    run_ready(cx);
    draw(cx, window);
    (window, renders)
}

fn draw(cx: &mut TestAppContext, window: AnyWindowHandle) {
    cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx))
        .unwrap();
}

/// Moves the clock on by `by`, then draws what that woke.
fn wait(cx: &mut TestAppContext, window: AnyWindowHandle, by: Duration) {
    cx.executor().advance_clock(by);
    run_ready(cx);
    draw(cx, window);
}

fn sprites(cx: &mut TestAppContext, window: AnyWindowHandle) -> Vec<Bounds<ScaledPixels>> {
    cx.update_window(window, |_, window, _| {
        window
            .rendered_frame
            .scene
            .polychrome_sprites
            .iter()
            .map(|sprite| sprite.bounds)
            .collect()
    })
    .unwrap()
}

#[crate::test]
fn an_animated_image_asks_for_no_animation_frame(cx: &mut TestAppContext) {
    let (window, _) = open(cx, FRAME_DELAY);
    let requested = cx
        .update_window(window, |_, window, cx| window.simulate_next_frame(cx))
        .unwrap();
    assert_eq!(requested, 0);
}

#[crate::test]
fn an_animated_image_wakes_its_view_when_its_next_frame_is_due(cx: &mut TestAppContext) {
    let (window, renders) = open(cx, FRAME_DELAY);
    let first = sprites(cx, window);
    assert_eq!(first.len(), 1);
    let rendered = renders.get();

    wait(cx, window, FRAME_DELAY - MILLISECOND);
    assert_eq!(renders.get(), rendered, "woken before the frame is due");
    assert_eq!(sprites(cx, window), first);

    wait(cx, window, MILLISECOND);
    assert_eq!(renders.get(), rendered + 1, "woken once, when it is due");
    assert_ne!(sprites(cx, window), first, "on to the next frame");

    wait(cx, window, FRAME_DELAY);
    assert_eq!(renders.get(), rendered + 2, "and again for the frame after");
}

/// Frames with no delay, which GIFs often declare, wake the view every
/// `MIN_FRAME_DELAY` rather than as often as the executor runs.
#[crate::test]
fn frames_without_a_delay_wake_their_view_at_most_every_16_ms(cx: &mut TestAppContext) {
    let (window, renders) = open(cx, Duration::ZERO);
    for frame in 1..=3 {
        let rendered = renders.get();
        wait(cx, window, MIN_FRAME_DELAY - MILLISECOND);
        assert_eq!(renders.get(), rendered, "frame {frame} woke early");
        wait(cx, window, MILLISECOND);
        assert_eq!(renders.get(), rendered + 1, "frame {frame} didn't wake");
    }
}

/// A feed: a scroll container of rows, the second holding an animated image
/// in a view of its own, as a chat holds an embed.
struct Feed {
    gif: Entity<Gif>,
}

impl Render for Feed {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().bg(rgb(0xffffff)).child(
            div()
                .id("feed")
                .overflow_y_scroll()
                .w(px(200.))
                .h(px(100.))
                .children((0..30).map(|index| {
                    let row = div().h(px(30.)).bg(rgb(0x100000 + index * 0x10));
                    if index == 1 {
                        row.child(self.gif.clone())
                    } else {
                        row
                    }
                })),
        )
    }
}

fn open_feed(cx: &mut TestAppContext, layers: bool) -> WindowHandle<Feed> {
    let window = cx.add_window(|_, cx| Feed {
        gif: cx.new(|_| Gif {
            image: gif(FRAME_DELAY),
            renders: Rc::default(),
        }),
    });
    cx.update_window(window.into(), |_, window, _| {
        window.set_scroll_layers(layers);
        // Both windows play the image, as the active window does: the
        // platform has only one active window.
        window.active.set(true);
    })
    .unwrap();
    run_ready(cx);
    draw(cx, window.into());
    window
}

/// Whether the feed composited its layer in the last frame.
fn composited(window: &mut Window) -> bool {
    let id = window
        .fast_layers
        .scrolls
        .containers()
        .find(|id| id.last() == Some(&"feed".into()))
        .cloned()
        .expect("the scroll container was painted");
    crate::fast::layers::policy::last_decision(window, &id)
        == Some(crate::fast::layers::policy::Decision::Composite)
}

/// Scrolls a feed with layers and one without through the same frames while
/// the image in it plays: every frame draws the same in both, the image goes
/// through its frames, and the feed composites its layer in the frames where
/// the image doesn't change.
#[crate::test]
fn an_animated_image_follows_the_scroll_and_keeps_playing(cx: &mut TestAppContext) {
    if !crate::fast::layers::COMPILED {
        return;
    }
    let layered = open_feed(cx, true);
    let plain = open_feed(cx, false);
    let mut composited_frames = 0;
    let mut sizes = std::collections::BTreeSet::new();
    for (step, dy) in [-7., -5., -3., -6., -4., -2., 3., -5., -1., -8., -3., -4.]
        .into_iter()
        .enumerate()
    {
        if step % 4 == 3 {
            cx.executor().advance_clock(FRAME_DELAY);
            run_ready(cx);
        }
        let drawn = [layered, plain].map(|window| {
            cx.update_window(window.into(), |_, window, cx| {
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
                window.draw(cx).clear(cx);
                window.painted_primitives()
            })
            .unwrap()
        });
        assert_eq!(drawn[0], drawn[1], "step {step}, scrolled by {dy}");
        sizes.extend(
            sprites(cx, plain.into())
                .iter()
                .map(|bounds| format!("{:?}", bounds.size)),
        );
        if cx
            .update_window(layered.into(), |_, window, _| composited(window))
            .unwrap()
        {
            composited_frames += 1;
        }
    }
    assert_eq!(
        sizes.len(),
        3,
        "the image went through its frames: {sizes:?}"
    );
    assert!(
        composited_frames >= 6,
        "composited {composited_frames} of 12 frames"
    );
}
