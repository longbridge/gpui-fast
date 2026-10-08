//! A scrollbar drawn and driven the way GPUI Kit's `Scrollbar` is
//! (gpui-kit `crates/base/src/scrollbar.rs`), so the scroll scenarios can
//! measure what scrolling costs with the scrollbar every real list has.
//!
//! What it does as GPUI Kit's does, and what matters to a frame's cost:
//!
//! - it is an overlay over the scrolled content, painted by the view that
//!   owns the content, and reads the scroll offset while it is prepainted
//!   (`ScrollHandle::offset`, `ListState::scroll_px_offset_for_scrollbar`)
//!   to place the thumb;
//! - its wheel listener notifies that view again when the offset moved since
//!   the scrollbar was last prepainted;
//! - a thumb is dragged by a mouse-move listener (not by GPUI's drag and
//!   drop) that sets the offset (`ListState::set_offset_from_scrollbar` for a
//!   `list`) and notifies the view on every move; GPUI Kit throttles those
//!   notifications to 120 per second, which one move a frame never reaches;
//! - hovering the track notifies the view when the hover changes.
//!
//! Its fade and width animations, which run for 300–500 ms around a scroll,
//! are stood in for by [`State::animated`]: a thumb whose shade steps every
//! frame while it animates, asking for an animation frame from its prepaint
//! as GPUI Kit's does while it fades, counted in frames rather than wall
//! time.

use std::{cell::Cell, rc::Rc};

use gpui::{
    Bounds, Hsla, ListState, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
    Point, ScrollHandle, ScrollWheelEvent, UniformListScrollHandle, canvas, fill, hsla, point,
    prelude::*, px, size,
};

/// Width of the track, in logical pixels.
pub const TRACK_WIDTH: f32 = 12.;
/// Shortest the thumb gets.
const MIN_THUMB: f32 = 20.;

/// What a scrollbar scrolls, and how GPUI Kit reads and sets its offset.
#[derive(Clone)]
pub enum Scrolled {
    Div(ScrollHandle),
    UniformList(UniformListScrollHandle),
    List(ListState),
}

impl Scrolled {
    fn offset(&self) -> Point<Pixels> {
        match self {
            Scrolled::Div(handle) => handle.offset(),
            Scrolled::UniformList(handle) => handle.0.borrow().base_handle.offset(),
            Scrolled::List(state) => state.scroll_px_offset_for_scrollbar(),
        }
    }

    fn set_offset(&self, offset: Point<Pixels>) {
        match self {
            Scrolled::Div(handle) => handle.set_offset(offset),
            Scrolled::UniformList(handle) => handle.0.borrow_mut().base_handle.set_offset(offset),
            Scrolled::List(state) => state.set_offset_from_scrollbar(offset),
        }
    }

    /// The height of the content, the viewport's included.
    fn content_height(&self, viewport: Pixels) -> Pixels {
        match self {
            Scrolled::Div(handle) => handle.max_offset().y + handle.bounds().size.height,
            Scrolled::UniformList(handle) => {
                let handle = &handle.0.borrow().base_handle;
                handle.max_offset().y + handle.bounds().size.height
            }
            Scrolled::List(state) => viewport + state.max_offset_for_scrollbar().y,
        }
    }

    fn start_drag(&self) {
        if let Scrolled::List(state) = self {
            state.scrollbar_drag_started();
        }
    }

    fn end_drag(&self) {
        if let Scrolled::List(state) = self {
            state.scrollbar_drag_ended();
        }
    }
}

/// What a scrollbar keeps from frame to frame.
#[derive(Clone, Copy, Default)]
pub struct State {
    /// The offset when the scrollbar was last prepainted.
    last_offset: Point<Pixels>,
    /// Where on the thumb it was grabbed, while it is dragged.
    grab: Option<Pixels>,
    hovered: bool,
    /// On how many frames of every 100 the thumb's shade steps, asking for
    /// an animation frame from the scrollbar's prepaint.
    animated_frames: u32,
    /// The frames the scrollbar was prepainted in.
    tick: u32,
}

impl State {
    /// A scrollbar animating on the first `frames` of every 100 frames it is
    /// drawn in, as GPUI Kit's does while it fades in and out.
    pub fn animated(frames: u32) -> Self {
        Self {
            animated_frames: frames,
            ..Self::default()
        }
    }

    /// Whether the thumb is animating.
    fn animating(&self) -> bool {
        self.tick % 100 < self.animated_frames
    }
}

/// The thumb of a track of `track` bounds over content of `content` height,
/// scrolled down `scrolled`.
fn thumb(track: Bounds<Pixels>, content: Pixels, scrolled: Pixels) -> Option<Bounds<Pixels>> {
    let viewport = track.size.height;
    if content <= viewport {
        return None;
    }
    let length = (viewport * (viewport / content)).max(px(MIN_THUMB));
    let range = content - viewport;
    let top = track.origin.y + (viewport - length) * (scrolled / range).clamp(0., 1.);
    Some(Bounds {
        origin: point(track.origin.x, top),
        size: size(track.size.width, length),
    })
}

fn thumb_color(state: &State) -> Hsla {
    if state.grab.is_some() || state.hovered {
        hsla(0., 0., 0., 0.45)
    } else if state.animating() {
        hsla(0., 0., 0., 0.15 + 0.01 * (state.tick % 20) as f32)
    } else {
        hsla(0., 0., 0., 0.25)
    }
}

/// A vertical scrollbar over the bounds it is given, for `scrolled`, keeping
/// its state in `state`.
pub fn scrollbar(scrolled: Scrolled, state: Rc<Cell<State>>) -> impl IntoElement {
    canvas(
        {
            let scrolled = scrolled.clone();
            let state = state.clone();
            move |bounds, window, _| {
                // Placing the thumb reads the offset, as GPUI Kit's
                // `Scrollbar::prepaint` does.
                let offset = scrolled.offset();
                let mut current = state.get();
                if offset != current.last_offset {
                    current.last_offset = offset;
                }
                current.tick = current.tick.wrapping_add(1);
                if current.animating() {
                    window.request_animation_frame();
                }
                state.set(current);
                let track = Bounds {
                    origin: point(bounds.right() - px(TRACK_WIDTH), bounds.top()),
                    size: size(px(TRACK_WIDTH), bounds.size.height),
                };
                let content = scrolled.content_height(bounds.size.height);
                (bounds, track, thumb(track, content, -offset.y), content)
            }
        },
        move |_, (bounds, track, thumb_bounds, content), window, _| {
            let Some(thumb_bounds) = thumb_bounds else {
                return;
            };
            window.paint_quad(
                fill(thumb_bounds.dilate(px(-3.)), thumb_color(&state.get())).corner_radii(px(3.)),
            );
            let view = window.current_view();

            window.on_mouse_event({
                let scrolled = scrolled.clone();
                let state = state.clone();
                move |event: &ScrollWheelEvent, phase, _, cx| {
                    if phase.bubble() && bounds.contains(&event.position) {
                        let offset = scrolled.offset();
                        let mut current = state.get();
                        if offset != current.last_offset {
                            current.last_offset = offset;
                            state.set(current);
                            cx.notify(view);
                        }
                    }
                }
            });

            window.on_mouse_event({
                let scrolled = scrolled.clone();
                let state = state.clone();
                move |event: &MouseDownEvent, phase, _, cx| {
                    if phase.bubble()
                        && event.button == MouseButton::Left
                        && thumb_bounds.contains(&event.position)
                    {
                        cx.stop_propagation();
                        scrolled.start_drag();
                        let mut current = state.get();
                        current.grab = Some(event.position.y - thumb_bounds.top());
                        state.set(current);
                        cx.notify(view);
                    }
                }
            });

            window.on_mouse_event({
                let scrolled = scrolled.clone();
                let state = state.clone();
                move |event: &MouseMoveEvent, _, _, cx| {
                    let mut current = state.get();
                    let hovered = track.contains(&event.position);
                    let mut notify = hovered != current.hovered;
                    current.hovered = hovered;
                    if let Some(grab) = current.grab
                        && event.dragging()
                    {
                        cx.stop_propagation();
                        let travel = track.size.height - thumb_bounds.size.height;
                        let fraction =
                            ((event.position.y - grab - track.top()) / travel).clamp(0., 1.);
                        let target = -(content - track.size.height) * fraction;
                        let offset = scrolled.offset();
                        if offset.y != target {
                            scrolled.set_offset(point(offset.x, target));
                            notify = true;
                        }
                    }
                    state.set(current);
                    if notify {
                        cx.notify(view);
                    }
                }
            });

            window.on_mouse_event({
                move |_: &MouseUpEvent, phase, _, cx| {
                    let mut current = state.get();
                    if phase.bubble() && current.grab.is_some() {
                        current.grab = None;
                        state.set(current);
                        scrolled.end_drag();
                        cx.notify(view);
                    }
                }
            });
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}
